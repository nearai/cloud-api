//! The `Placer` facade: ties `rules.rs`, `score.rs`, and `affinity.rs`
//! together into a single placement decision, plus the `DecisionRecord` the
//! caller logs (IDs and numbers only — see the crate's privacy rules).
//!
//! A decision picks one replica slot (`host#index`), scoring every eligible
//! slot on its own state and its own pending load. The rules run in two
//! stages: the per-replica rules, then the heavy lane over their survivors,
//! judged against a lane view of every live replica.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use rand::Rng;

use crate::affinity::{pin_id, select, AffinityKey, PinId, Selection};
use crate::consts::FRESH_MAX_MS;
use crate::policy::{classify, lane_admits, lane_view, Class, LaneView, PriorityBand, Tier};
use crate::rules::{first_exclusion, saturated, Exclusion, Rule, ALL_RULES};
use crate::score::{
    effective_backlog, fleet_median_tps, known_idle, pending_for, replica_score, Pending,
};
use crate::snapshot::{ReplicaView, SlotId, Snapshot};
use crate::tuning::Tuning;

/// Per-request inputs the eligibility rules (`rules.rs`) check a
/// [`crate::snapshot::ReplicaView`] against, plus the rest of what
/// `Placer::place` needs to score and pick a slot.
///
/// `prompt_tokens` and `prefill_heavy` come from the pool's
/// `PlacementContext`; placement never estimates a request's size itself.
/// Sizing is prompt-only: the output allowance influences nothing here.
///
/// `affinity` holds an [`AffinityKey`], which has no `Debug`/`Display`, so
/// `PlaceInput` implements `Debug` manually and redacts it (see the manual
/// `impl` below) rather than deriving it.
#[derive(Clone)]
pub struct PlaceInput {
    pub model: String,
    /// Input tokens: prefill cost and lane load, and the requirement
    /// `Rule::Context` checks against each replica's engine
    /// `max_context_tokens`.
    pub prompt_tokens: u64,
    /// The lane class: `prompt_tokens` exceeds the base tier's capacity.
    /// Lane admission, heavy-pin continuity and heavy pin writes follow it.
    pub prefill_heavy: bool,
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
            .field("prefill_heavy", &self.prefill_heavy)
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
    /// The `x-session-id` request header.
    Header,
    /// The body's `session_id`.
    BodySessionId,
    /// The body's `prompt_cache_key`.
    PromptCacheKey,
    Prefix,
    #[default]
    None,
}

impl AffinitySource {
    /// A stable, content-free name for logs and metric tags: the three
    /// client-supplied sources share `client`.
    pub const fn as_str(self) -> &'static str {
        match self {
            AffinitySource::Header
            | AffinitySource::BodySessionId
            | AffinitySource::PromptCacheKey => "client",
            AffinitySource::Prefix => "prefix",
            AffinitySource::None => "none",
        }
    }

    /// Where the key came from, finer than [`Self::as_str`], for the decision
    /// log.
    pub const fn key_source(self) -> &'static str {
        match self {
            AffinitySource::Header => "header",
            AffinitySource::BodySessionId => "body_session_id",
            AffinitySource::PromptCacheKey => "prompt_cache_key",
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
    NoState,
    Stale,
    NoneEligible,
    /// Placement had state but every eligible replica was busy or capped; the
    /// request is routed by legacy (load-blind) routing. No stage-1 survivor
    /// and every live replica saturated (`rules::saturated`), for short and
    /// heavy requests alike.
    CapacityFull,
    /// Placement had state but every eligible replica was busy or capped; the
    /// request is routed by legacy (load-blind) routing. A heavy request on
    /// base where the lane filter left no candidate.
    LaneFull,
    /// Placement had state but every eligible replica was busy or capped; the
    /// request is routed by legacy (load-blind) routing. A heavy request on
    /// long where the backlog cap left no candidate.
    LongFull,
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
            LegacyReason::NoState => "no_state",
            LegacyReason::Stale => "stale",
            LegacyReason::NoneEligible => "none_eligible",
            LegacyReason::CapacityFull => "capacity_full",
            LegacyReason::LaneFull => "lane_full",
            LegacyReason::LongFull => "long_full",
            LegacyReason::HostUnmapped => "host_unmapped",
            LegacyReason::KeyGroup => "key_group",
            LegacyReason::Incomplete => "incomplete",
        }
    }
}

/// The outcome of a placement decision. Holds a `PinId` (no `Debug`), so
/// this type intentionally does not derive `Debug`.
///
/// `Legacy` means the caller falls back to legacy routing: there is no usable
/// state, or there is state but no capacity (see [`LegacyReason`]).
pub enum Decision {
    Place {
        slot: SlotId,
        record: DecisionRecord,
        pin_write: Option<(PinId, SlotId)>,
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
            Decision::Place { record, .. } | Decision::Legacy { record, .. } => record,
        }
    }

    fn record_mut(&mut self) -> &mut DecisionRecord {
        match self {
            Decision::Place { record, .. } | Decision::Legacy { record, .. } => record,
        }
    }
}

/// A content-free record of a placement decision, safe to log as-is (IDs
/// and numbers only). Must never carry `AffinityKey` or `PinId` bytes/hex.
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    /// `place` or `legacy`.
    pub outcome: &'static str,
    /// The `LegacyReason::as_str`, for legacy decisions.
    pub reason: Option<&'static str>,
    pub tier: Tier,
    /// The lane class, from `PlaceInput::prefill_heavy`.
    pub class: Class,
    /// `RoutePolicy::as_str`, for placed decisions.
    pub strategy: Option<&'static str>,
    /// `PriorityBand::as_str`.
    pub priority_band: &'static str,
    /// Lane members and the lane cap, as seen by this decision (0 when it
    /// never got as far as the lane).
    pub lane_size: u16,
    pub lane_cap: u16,
    pub prompt_tokens: u64,
    /// Time spent in `Placer::place`, in microseconds.
    pub place_us: u32,
    pub rank: Option<u8>,
    pub affinity: &'static str,
    /// `AffinitySource::key_source`.
    pub key_source: &'static str,
    /// What became of the request's follow pin: `held`, `released_load`,
    /// `not_admitted`, `stale_boot` or `none`.
    pub pin_outcome: &'static str,
    /// Age of the live pin, in ms.
    pub pin_age_ms: Option<u64>,
    /// The loads `pin_holds` compared: the pinned slot's and the lightest
    /// other admitted candidate's (absent when there is none).
    pub pinned_load: Option<u64>,
    pub best_other_load: Option<u64>,
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
            class: Class::of(input.prefill_heavy),
            strategy: None,
            priority_band: PriorityBand::of(input.priority).as_str(),
            lane_size: 0,
            lane_cap: 0,
            prompt_tokens: input.prompt_tokens,
            place_us: 0,
            rank: None,
            affinity: input.affinity_source.as_str(),
            key_source: input.affinity_source.key_source(),
            pin_outcome: "none",
            pin_age_ms: None,
            pinned_load: None,
            best_other_load: None,
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

/// A stage-1 survivor, the pending load it is scored with, its lane load
/// (`effective_backlog + pending.tok`), and whether that load is known to be
/// zero (`known_idle` with nothing pending), for the lane's idle waiver.
struct Candidate<'a> {
    view: &'a ReplicaView,
    pending: Pending,
    load: u64,
    idle: bool,
}

/// The load test on a request's follow pin.
struct PinTest {
    holds: bool,
    pinned_load: u64,
    /// The lightest other admitted candidate's load, if there is one.
    best_other_load: Option<u64>,
}

/// Pin continuity for a request pinned to `pin`: `None` when `pin` is not an
/// admitted candidate, else whether it holds.
///
/// The pin holds iff `pinned_load <= best_other_load + prompt * factor`
/// (`Tuning::pin_hold_factor`), with `load` the lane's (effective backlog
/// plus pending tokens) and `best_other_load` the lightest other admitted
/// candidate. In words: stay on the warm replica unless waiting behind its
/// backlog costs more than a cold prefill of the prompt on the lightest
/// alternative. With no other candidate, it holds.
fn pin_holds(
    candidates: &[Candidate<'_>],
    pin: &SlotId,
    prompt: u64,
    factor: f64,
) -> Option<PinTest> {
    let pinned = candidates.iter().find(|c| c.view.slot == *pin)?;
    let best_other_load = candidates
        .iter()
        .filter(|c| c.view.slot != *pin)
        .map(|c| c.load)
        .min();
    let holds = best_other_load
        .is_none_or(|other| pinned.load as f64 <= other as f64 + prompt as f64 * factor);
    Some(PinTest {
        holds,
        pinned_load: pinned.load,
        best_other_load,
    })
}

/// The pure placement decision-maker for one Fleet. Holds the deployment's
/// pin secret, the Fleet's tier and a handle to the live [`Tuning`] — no I/O.
/// The handle is written by the caller (an admin setting reload); a decision
/// reads it once, so one decision sees one consistent tuning.
pub struct Placer {
    pin_secret: [u8; 32],
    tier: Tier,
    tuning: Arc<ArcSwap<Tuning>>,
}

impl Placer {
    /// A placer on the default tuning, which nothing changes.
    pub fn new(pin_secret: [u8; 32], tier: Tier) -> Self {
        Self::with_tuning(
            pin_secret,
            tier,
            Arc::new(ArcSwap::from_pointee(Tuning::default())),
        )
    }

    /// A placer reading `tuning`, shared with whoever updates it.
    pub fn with_tuning(pin_secret: [u8; 32], tier: Tier, tuning: Arc<ArcSwap<Tuning>>) -> Self {
        Self {
            pin_secret,
            tier,
            tuning,
        }
    }

    /// The live tuning handle this placer reads.
    pub fn tuning_handle(&self) -> &Arc<ArcSwap<Tuning>> {
        &self.tuning
    }

    /// The capacity tier this placer serves.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Decide where `input` goes on this Fleet, given the latest snapshot and
    /// the part of this node's own ledger that `snap.routed` cannot include
    /// yet (`mine`, per slot: [`crate::score::unseen_by_read`] of the slot's
    /// ledger against `snap.routed_read_ms`; see
    /// [`crate::score::pending_for`]). `Legacy` when the snapshot is
    /// disabled, empty or stale, or when nothing survives the rules (see
    /// [`LegacyReason`]); otherwise `Place`. The record's `place_us` times
    /// the whole call.
    pub fn place(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        mine: &HashMap<SlotId, Pending>,
        rng: &mut impl Rng,
    ) -> Decision {
        let started = Instant::now();
        let tuning = **self.tuning.load();
        let mut decision = self.decide(input, snap, mine, &tuning, rng);
        decision.record_mut().place_us =
            u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        decision
    }

    fn decide(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        mine: &HashMap<SlotId, Pending>,
        tuning: &Tuning,
        rng: &mut impl Rng,
    ) -> Decision {
        if snap.disabled {
            return self.legacy(input, snap, LegacyReason::Disabled, None);
        }

        if snap.replicas.is_empty() {
            return self.legacy(input, snap, LegacyReason::NoState, None);
        }

        if input.now_ms.saturating_sub(snap.built_ms) > FRESH_MAX_MS {
            return self.legacy(input, snap, LegacyReason::Stale, None);
        }

        // Stage 1: per-replica rules, tallying exclusions for the record.
        // Every live replica (past Lifecycle and Freshness) also feeds the
        // lane view with its load, survivor or not.
        let mut tally = Tally::default();
        let mut live: Vec<(&ReplicaView, u64)> = Vec::with_capacity(snap.replicas.len());
        let mut candidates: Vec<Candidate<'_>> = Vec::with_capacity(snap.replicas.len());
        for view in &snap.replicas {
            let exclusion = first_exclusion(view, input, input.now_ms, tuning);
            if let Some(Exclusion(rule @ (Rule::Lifecycle | Rule::Freshness))) = exclusion {
                tally.add(rule);
                continue;
            }
            let pending = pending_for(
                snap.routed.get(&view.slot),
                mine.get(&view.slot).copied().unwrap_or_default(),
            );
            let load = effective_backlog(&view.state.load).saturating_add(pending.tok);
            live.push((view, load));
            match exclusion {
                None => {
                    let idle = known_idle(&view.state.load) && pending == Pending::default();
                    candidates.push(Candidate {
                        view,
                        pending,
                        load,
                        idle,
                    });
                }
                Some(Exclusion(rule)) => tally.add(rule),
            }
        }

        // Stage 2: the heavy lane, admitting stage-1 survivors against a view
        // of every live replica.
        let class = Class::of(input.prefill_heavy);
        let lane = {
            let survivors: Vec<&SlotId> = candidates.iter().map(|c| &c.view.slot).collect();
            lane_view(&live, &survivors, tuning.lane_load_tokens)
        };

        // No stage-1 survivor is a state problem (stale, not ready, over
        // context, no load counts), so `NoneEligible`, except when every live
        // replica reported itself full: that is a capacity answer, for short
        // and heavy requests alike. Either way the caller falls back.
        if candidates.is_empty() {
            let all_full =
                !live.is_empty() && live.iter().all(|(v, _)| saturated(v, tuning.kv_max));
            let reason = if all_full {
                LegacyReason::CapacityFull
            } else {
                LegacyReason::NoneEligible
            };
            return self.legacy_in_lane(input, snap, &lane, reason, tally.record());
        }

        candidates.retain(|c| {
            let is_member = lane.members.contains(&c.view.slot);
            let admitted = lane_admits(
                self.tier,
                class,
                c.load,
                c.idle,
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
            // Only the lane excluded the survivors. `lane_admits` always
            // admits a short request when anything survived stage 1, so this
            // is heavy in practice; a short request would still fall back.
            let reason = match (class, self.tier) {
                (Class::Heavy, Tier::Long) => LegacyReason::LongFull,
                (Class::Heavy, Tier::Base) => LegacyReason::LaneFull,
                (Class::Short, _) => LegacyReason::NoneEligible,
            };
            return self.legacy_in_lane(input, snap, &lane, reason, tally.record());
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
        // A pin written on an earlier boot of its host points at a cold
        // cache: it is ignored, and rewritten wherever this request lands.
        let mut stale_boot_pin = false;
        let pin_lookup: Option<(SlotId, u64)> = pin_id_opt.as_ref().and_then(|pid| {
            let (slot, at_ms, boot) = snap.pins.get_with_boot(pid, input.now_ms)?;
            if snap.pin_boot_current(&slot.host, boot) {
                Some((slot.clone(), at_ms))
            } else {
                stale_boot_pin = true;
                None
            }
        });

        // Pin continuity is judged on load, not score (see `pin_holds`). A
        // pin that holds wins whatever its score; one that is released leaves
        // the walk entirely, so the HRW walk cannot land back on the slot the
        // load test just moved it off.
        let pin_slot = pin_lookup.as_ref().map(|(s, _)| s);
        let pin_test = pin_slot
            .and_then(|p| pin_holds(&candidates, p, input.prompt_tokens, tuning.pin_hold_factor));
        let pin_outcome = match (&pin_test, pin_slot) {
            (Some(t), _) if t.holds => "held",
            (Some(_), _) => "released_load",
            (None, Some(_)) => "not_admitted",
            (None, None) if stale_boot_pin => "stale_boot",
            (None, None) => "none",
        };
        let released: Vec<(SlotId, f64)>;
        let walk: &[(SlotId, f64)] = match (&pin_test, pin_slot) {
            (Some(t), Some(p)) if !t.holds => {
                released = scores.iter().filter(|(s, _)| s != p).cloned().collect();
                &released
            }
            _ => &scores,
        };

        let selected = match select(
            input.affinity.as_ref(),
            pin_slot,
            pin_test.as_ref().is_some_and(|t| t.holds),
            walk,
            tuning,
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

        // A keyed prompt-heavy placement always (re)writes its pin: lane membership
        // follows the prefill backlog, so once it drains, the survivor set
        // and the HRW walk over it can change, and only a pin keeps the next
        // turn on its warm replica. Short requests pin only on a move or a
        // half-TTL refresh.
        let mut pin_write: Option<(PinId, SlotId)> = None;
        if let Some(pid) = pin_id_opt {
            let should_write = input.prefill_heavy
                || stale_boot_pin
                || match selected.selection {
                    Selection::Pinned => pin_lookup
                        .as_ref()
                        .map(|(_, at_ms)| {
                            input.now_ms.saturating_sub(*at_ms) > snap.pins.ttl_ms() / 2
                        })
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
            strategy: Some(classify(self.tier, class, &lane, &selected.slot).as_str()),
            priority_band: PriorityBand::of(input.priority).as_str(),
            lane_size: u16::try_from(lane.size).unwrap_or(u16::MAX),
            lane_cap: u16::try_from(lane.cap).unwrap_or(u16::MAX),
            prompt_tokens: input.prompt_tokens,
            place_us: 0,
            rank,
            affinity: input.affinity_source.as_str(),
            key_source: input.affinity_source.key_source(),
            pin_outcome,
            pin_age_ms: pin_lookup
                .as_ref()
                .map(|(_, at_ms)| input.now_ms.saturating_sub(*at_ms)),
            pinned_load: pin_test.as_ref().map(|t| t.pinned_load),
            best_other_load: pin_test.as_ref().and_then(|t| t.best_other_load),
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

    /// A `Decision::Legacy` that also carries the lane's size and cap, for a
    /// fall-back decided after the lane was built.
    fn legacy_in_lane(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        lane: &LaneView,
        reason: LegacyReason,
        excluded: [(Rule, u16); 5],
    ) -> Decision {
        let mut decision = self.legacy(input, snap, reason, Some(excluded));
        let record = decision.record_mut();
        record.lane_size = u16::try_from(lane.size).unwrap_or(u16::MAX);
        record.lane_cap = u16::try_from(lane.cap).unwrap_or(u16::MAX);
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::{hrw_rank, AffinityKey};
    use crate::consts::{FRESH_MAX_MS, PIN_TTL_MS};
    use crate::consts::{
        HEAVY_BACKLOG_CAP, HEAVY_BASE_MAX_PROMPT, LANE_LOAD_TOKENS, LONG_BACKLOG_CAP,
    };
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
            host_boots: HashMap::new(),
            host_reported_ms: HashMap::new(),
        }
    }

    fn placer() -> Placer {
        Placer::new([1u8; 32], Tier::Base)
    }

    fn keyed(key: AffinityKey) -> PlaceInput {
        let mut input = base_input();
        input.affinity = Some(key);
        input.affinity_source = AffinitySource::Header;
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
        }
    }

    fn legacy_reason(d: Decision) -> (LegacyReason, DecisionRecord) {
        match d {
            Decision::Legacy { reason, record } => (reason, record),
            Decision::Place { .. } => panic!("expected Legacy, got Place"),
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
    fn busy_or_capped_legacy_reasons_have_stable_names() {
        assert_eq!(LegacyReason::CapacityFull.as_str(), "capacity_full");
        assert_eq!(LegacyReason::LaneFull.as_str(), "lane_full");
        assert_eq!(LegacyReason::LongFull.as_str(), "long_full");
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

        let mut other = base_input();
        other.model = "some-other-model".into();
        let (reason, _) = legacy_reason(placer().place(&other, &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::Disabled);
    }

    #[test]
    fn any_model_whose_hosts_publish_is_placed() {
        // There is no model allow-list: a snapshot holds only the Fleet's own
        // attested hosts, so any model whose hosts publish frames is placed.
        let mut input = base_input();
        input.model = "acme/any-new-model".into();
        let snap = snap_with(vec![ready_view("gpu01", 0)]);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) = placed(placer().place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.outcome, "place");
    }

    #[test]
    fn model_without_publishing_hosts_is_legacy() {
        // A model whose hosts never publish has an empty snapshot and stays
        // on legacy routing, whatever its name.
        let mut input = base_input();
        input.model = "acme/any-new-model".into();
        let snap = snap_with(vec![]);
        let mut rng = StdRng::seed_from_u64(1);
        let (reason, record) =
            legacy_reason(placer().place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NoState);
        assert_eq!(record.reason, Some("no_state"));
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
        input.prompt_tokens = 150_000;
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
    fn pin_from_previous_boot_is_ignored() {
        let secret = [4u8; 32];
        let slots = vec![slot("gpu01", 0), slot("gpu02", 0)];
        let key = find_key_with_home(&slots, &slot("gpu02", 0));
        let pid = pin_id(Tier::Base, &key, &secret);
        let pinned_on = |boot: &str| {
            let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
            snap.host_boots = HashMap::from([
                ("gpu01".to_string(), "boot-b".to_string()),
                ("gpu02".to_string(), "boot-x".to_string()),
            ]);
            std::sync::Arc::make_mut(&mut snap.pins).insert_on_boot(
                *pid.as_bytes(),
                slot("gpu01", 0),
                NOW - 1_000,
                Some(boot.to_string()),
            );
            snap
        };
        let place = |snap: &Snapshot| {
            let mut rng = StdRng::seed_from_u64(1);
            placed(Placer::new(secret, Tier::Base).place(
                &keyed(key.clone()),
                snap,
                &HashMap::new(),
                &mut rng,
            ))
        };

        // Written on the host's current boot: honoured.
        let (chosen, record, _) = place(&pinned_on("boot-b"));
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.selection, Some("pinned"));

        // Written on a boot the host has moved on from: its cache is cold,
        // so the pin is ignored, HRW picks home and a fresh pin is written.
        let (chosen, record, pin_write) = place(&pinned_on("boot-a"));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.selection, Some("home"));
        assert_eq!(record.pinned, None);
        let (_, rewritten) = pin_write.expect("a fresh pin is written");
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

    /// A prompt-heavy request.
    fn heavy(prompt_tokens: u64) -> PlaceInput {
        let mut input = base_input();
        input.prefill_heavy = true;
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

    /// The record of a `Legacy` decision that must carry `want`.
    fn legacy_record(d: Decision, want: LegacyReason) -> DecisionRecord {
        let (reason, record) = legacy_reason(d);
        assert_eq!(reason, want);
        assert_eq!(record.outcome, "legacy");
        assert_eq!(record.reason, Some(want.as_str()));
        assert_eq!(record.strategy, None);
        record
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
                Decision::Legacy { reason, record } => {
                    assert_eq!(reason, LegacyReason::LaneFull);
                    assert_eq!(record.excluded[4], (Rule::Lane, 8));
                    assert_eq!((record.lane_size, record.lane_cap), (2, 2));
                    strategies.push(reason.as_str());
                }
            }
        }
        // Two new members, each joined once more until the next prompt would
        // take it over HEAVY_BACKLOG_CAP (3 x 120K > 300K), then legacy.
        assert_eq!(
            strategies,
            [
                "heavy_lane_admit",
                "heavy_lane_admit",
                "heavy_lane_join",
                "heavy_lane_join",
                "lane_full",
                "lane_full",
                "lane_full",
                "lane_full"
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
    fn long_tier_falls_back_for_heavy_when_all_over_cap() {
        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![
            with_backlog("long01", 0, 550_000),
            with_backlog("long01", 1, 550_000),
        ]);
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            long.place(&heavy(100_000), &snap, &HashMap::new(), &mut rng),
            LegacyReason::LongFull,
        );
        assert_eq!(record.excluded[4], (Rule::Lane, 2));

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
    fn long_tier_third_oversized_prompt_in_burst_falls_back() {
        // Two idle long replicas and three 310K prompts in a burst, each
        // placement recorded in this node's ledger before the next. The long
        // tier has no lane cap, only LONG_BACKLOG_CAP (600K): the first lands
        // on an idle replica, the second can't join it (620K > 600K) and takes
        // the other, and the third fits neither, so it falls back to legacy.
        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![ready_view("long01", 0), ready_view("long01", 1)]);
        let prompt = 310_000;
        let mut mine: HashMap<SlotId, Pending> = HashMap::new();
        let mut chosen = Vec::new();
        for seed in 0..2 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (slot_, record, _) = placed(long.place(&heavy(prompt), &snap, &mine, &mut rng));
            assert_eq!(record.strategy, Some("heavy_long"));
            assert_eq!(record.excluded[4], (Rule::Lane, seed as u16));
            let p = mine.entry(slot_.clone()).or_default();
            p.req += 1;
            p.tok += prompt;
            chosen.push(slot_);
        }
        assert_ne!(chosen[0], chosen[1], "one prompt per replica");

        let mut rng = StdRng::seed_from_u64(2);
        let record = legacy_record(
            long.place(&heavy(prompt), &snap, &mine, &mut rng),
            LegacyReason::LongFull,
        );
        assert_eq!(record.excluded[4], (Rule::Lane, 2));
    }

    #[test]
    fn heavy_with_all_replicas_stale_is_none_eligible() {
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
    fn placer_reads_live_tuning_per_decision() {
        let mut v = ready_view("gpu01", 0);
        v.state.load.kv_usage = Some(0.8);
        let snap = snap_with(vec![v]);
        let handle = Arc::new(ArcSwap::from_pointee(Tuning::default()));
        let p = Placer::with_tuning([1u8; 32], Tier::Base, handle.clone());
        let mut rng = StdRng::seed_from_u64(1);

        placed(p.place(&base_input(), &snap, &HashMap::new(), &mut rng));

        // Tightening kv_max through the shared handle takes effect on the
        // next decision, with no new Placer.
        handle.store(Arc::new(Tuning {
            kv_max: 0.7,
            ..Tuning::default()
        }));
        let (reason, _) = legacy_reason(p.place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::CapacityFull);
    }

    #[test]
    fn all_replicas_kv_full_is_capacity_full_for_any_class() {
        // Every live replica is out of capacity (KV at the bound), so there is
        // no survivor and the answer is `capacity_full` for a short request
        // as much as a heavy one: legacy routing, with the reason named.
        let mut kv_full = ready_view("gpu01", 0);
        kv_full.state.load.kv_usage = Some(crate::consts::KV_MAX);
        let mut kv_over = ready_view("gpu01", 1);
        kv_over.state.load.kv_usage = Some(0.99);
        let mut draining = ready_view("gpu02", 0);
        draining.state.lifecycle_state = crate::frame::Lifecycle::Draining;
        let snap = snap_with(vec![kv_full.clone(), kv_over, draining]);
        for tier in [Tier::Base, Tier::Long] {
            let mut rng = StdRng::seed_from_u64(1);
            let p = Placer::new([1u8; 32], tier);
            for input in [heavy(100_000), base_input()] {
                let record = legacy_record(
                    p.place(&input, &snap, &HashMap::new(), &mut rng),
                    LegacyReason::CapacityFull,
                );
                assert_eq!(record.excluded[0], (Rule::Lifecycle, 1));
                assert_eq!(record.excluded[2], (Rule::Capacity, 2));
                assert_eq!(record.excluded[4], (Rule::Lane, 0));
            }
        }

        // Deep queues with low KV are not saturation: the replica survives.
        let mut queued = ready_view("gpu01", 1);
        queued.state.limits.max_running = Some(4);
        queued.state.load.running = Some(8);
        queued.state.load.queued = Some(8);
        let snap = snap_with(vec![kv_full.clone(), queued]);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, _, _) =
            placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("gpu01", 1));

        let mut rng = StdRng::seed_from_u64(1);
        // A live replica missing load counts is a state problem, not a full
        // one: `none_eligible`, for a missing backlog as much as missing
        // running/queued.
        let mut countless = ready_view("gpu02", 1);
        countless.state.load.running = None;
        countless.state.load.queued = None;
        countless.state.load.prefill_backlog_tokens = None;
        let mut no_backlog = ready_view("gpu02", 1);
        no_backlog.state.load.prefill_backlog_tokens = None;
        for missing in [countless, no_backlog] {
            let snap = snap_with(vec![kv_full.clone(), missing]);
            let (reason, _) =
                legacy_reason(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
            assert_eq!(reason, LegacyReason::NoneEligible);
        }

        // So is a live replica the request does not fit (Context).
        let mut small = ready_view("gpu02", 1);
        small.state.limits.max_context_tokens = Some(64_000);
        let snap = snap_with(vec![kv_full, small]);
        let (reason, _) =
            legacy_reason(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(reason, LegacyReason::NoneEligible);
    }

    #[test]
    fn prompt_heavy_request_uses_lane() {
        let input = heavy(120_000);
        let mut rng = StdRng::seed_from_u64(1);
        let (_, record, _) =
            placed(placer().place(&input, &snap_with(eight_slots()), &HashMap::new(), &mut rng));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));
        assert_eq!(record.class, Class::Heavy);
        legacy_record(
            placer().place(&input, &saturated_base(), &HashMap::new(), &mut rng),
            LegacyReason::LaneFull,
        );
    }

    #[test]
    fn heavy_with_no_views_is_no_state() {
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
    fn short_request_on_long_tier_overflows_not_legacy() {
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
        // The pin's 100K backlog is no more than a cold 100K prefill on an
        // idle replica (the load bound is inclusive), so the pin holds even
        // though its score is far outside the affinity bound.
        let secret = [8u8; 32];
        let (snap, key, pinned) = pinned_member_with_room(secret, 100_000);
        let mut input = keyed(key);
        input.prefill_heavy = true;
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
        input.prefill_heavy = true;
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
        let input = heavy(LONG_BACKLOG_CAP + 100_000);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) = placed(long.place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("long01", 1));
        assert_eq!(record.strategy, Some("heavy_long"));
        assert_eq!(record.excluded[4], (Rule::Lane, 1));
    }

    #[test]
    fn member_excluded_for_kv_still_counts_toward_lane() {
        // 4 live replicas (lane cap 1). gpu01#0 carries a 100K backlog but is
        // KV-full, so stage 1 drops it; it still holds the one lane slot, and
        // a heavy request can't open a second member on an idle replica.
        let mut full = with_backlog("gpu01", 0, 100_000);
        full.state.load.kv_usage = Some(0.96);
        let snap = snap_with(vec![
            full,
            ready_view("gpu01", 1),
            ready_view("gpu02", 0),
            ready_view("gpu02", 1),
        ]);
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng),
            LegacyReason::LaneFull,
        );
        assert_eq!((record.lane_size, record.lane_cap), (1, 1));
        assert_eq!(record.excluded[2], (Rule::Capacity, 1));
        assert_eq!(record.excluded[4], (Rule::Lane, 3));

        // Short requests still see the idle survivors as clean.
        let (_, record, _) =
            placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
        assert_eq!(record.strategy, Some("short_clean"));
        assert_eq!(record.eligible, 3);

        // A stale member is not live: it neither holds a lane slot nor counts
        // toward the cap.
        let mut stale = with_backlog("gpu01", 0, 100_000);
        stale.state.engine_sampled_at_ms = Some(NOW - FRESH_MAX_MS - 1);
        let snap = snap_with(vec![
            stale,
            ready_view("gpu01", 1),
            ready_view("gpu02", 0),
            ready_view("gpu02", 1),
        ]);
        let (_, record, _) =
            placed(placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!((record.lane_size, record.lane_cap), (0, 1));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));
    }

    #[test]
    fn missing_backlog_is_excluded_not_estimated() {
        // No reported backlog, 40 queued: the replica is dropped by
        // `Rule::Capacity` (missing evidence), never scored or admitted on an
        // estimate, and it counts as no lane member (zero load).
        let mut unknown = ready_view("gpu01", 0);
        unknown.state.load.queued = Some(40);
        unknown.state.load.prefill_backlog_tokens = None;
        let snap = snap_with(vec![
            unknown.clone(),
            ready_view("gpu01", 1),
            ready_view("gpu02", 0),
            ready_view("gpu02", 1),
        ]);
        for seed in 0..32 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (chosen, record, _) =
                placed(placer().place(&base_input(), &snap, &HashMap::new(), &mut rng));
            assert_ne!(chosen, slot("gpu01", 0), "seed {seed}");
            assert_eq!(record.lane_size, 0);
            assert_eq!(record.excluded[2], (Rule::Capacity, 1));
        }

        // Alone on the long tier it leaves nothing to place on.
        let long = Placer::new([1u8; 32], Tier::Long);
        unknown.slot = slot("long01", 0);
        let snap = snap_with(vec![unknown]);
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            long.place(&heavy(310_000), &snap, &HashMap::new(), &mut rng),
            LegacyReason::NoneEligible,
        );
        assert_eq!(record.excluded[2], (Rule::Capacity, 1));
    }

    #[test]
    fn idle_waiver_requires_known_load() {
        // A prompt over LONG_BACKLOG_CAP is admitted only on a replica whose
        // load is known to be zero: running, queued and backlog all reported
        // as 0. Missing any of them is not evidence of idleness, and
        // `Rule::Capacity` drops such a replica before the lane sees it.
        let long = Placer::new([1u8; 32], Tier::Long);
        let mut known = ready_view("long01", 0);
        known.state.load.prefill_backlog_tokens = Some(0);
        let mut no_backlog = ready_view("long01", 1);
        no_backlog.state.load.prefill_backlog_tokens = None;
        let mut no_running = ready_view("long01", 2);
        no_running.state.load.running = None;
        no_running.state.load.prefill_backlog_tokens = Some(0);
        let prompt = LONG_BACKLOG_CAP + 100_000;

        let snap = snap_with(vec![known, no_backlog.clone(), no_running.clone()]);
        for seed in 0..8 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (chosen, record, _) =
                placed(long.place(&heavy(prompt), &snap, &HashMap::new(), &mut rng));
            assert_eq!(chosen, slot("long01", 0), "seed {seed}");
            assert_eq!(record.excluded[2], (Rule::Capacity, 2));
        }

        // A known replica with any load is over the cap with this prompt.
        let busy = with_backlog("long01", 3, 1);
        let snap = snap_with(vec![busy, no_backlog, no_running]);
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            long.place(&heavy(prompt), &snap, &HashMap::new(), &mut rng),
            LegacyReason::LongFull,
        );
        assert_eq!(record.excluded[2], (Rule::Capacity, 2));
        assert_eq!(record.excluded[4], (Rule::Lane, 1));
    }

    /// Two long-tier replicas, `long01#0` pinned for the returned key with
    /// `pinned_backlog`, `long01#1` with `other_backlog`. The key's HRW home is
    /// the pinned slot, so only the pin rule can move it away.
    fn long_pinned_pair(
        secret: [u8; 32],
        pinned_backlog: u64,
        other_backlog: u64,
    ) -> (Snapshot, AffinityKey) {
        let pinned = slot("long01", 0);
        let views = vec![
            with_backlog("long01", 0, pinned_backlog),
            with_backlog("long01", 1, other_backlog),
        ];
        let all: Vec<SlotId> = views.iter().map(|v| v.slot.clone()).collect();
        let key = find_key_with_home(&all, &pinned);
        let mut snap = snap_with(views);
        let pid = pin_id(Tier::Long, &key, &secret);
        std::sync::Arc::make_mut(&mut snap.pins).insert(*pid.as_bytes(), pinned, NOW - 1_000);
        (snap, key)
    }

    fn keyed_heavy(key: AffinityKey, prompt_tokens: u64) -> PlaceInput {
        let mut input = keyed(key);
        input.prefill_heavy = true;
        input.prompt_tokens = prompt_tokens;
        input
    }

    #[test]
    fn heavy_pin_moves_when_backlog_exceeds_cold_prefill() {
        // The review's case: a 142K turn pinned behind a 450K backlog while
        // the other replica is idle. The lane admits the pin (592K <= 600K),
        // but waiting on 450K costs more than a cold 142K prefill elsewhere.
        let secret = [8u8; 32];
        let (snap, key) = long_pinned_pair(secret, 450_000, 0);
        let long = Placer::new(secret, Tier::Long);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) =
            placed(long.place(&keyed_heavy(key, 142_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("long01", 1));
        assert_eq!(record.pinned.as_deref(), Some("long01#0"));
        assert_eq!(
            record.excluded[4],
            (Rule::Lane, 0),
            "the lane admitted the pin"
        );
        assert_ne!(record.selection, Some("pinned"));
        let (_, rewritten) = pin_write.expect("the moved pin is rewritten");
        assert_eq!(rewritten, slot("long01", 1));

        // Pending tokens count as load too: a 450K routed-but-unreported
        // burst on the pin moves it the same way.
        let (mut snap, key) = long_pinned_pair(secret, 0, 0);
        snap.replicas[0].state.load.running = Some(1);
        let mut mine = HashMap::new();
        mine.insert(
            slot("long01", 0),
            Pending {
                req: 1,
                tok: 450_000,
            },
        );
        let (chosen, _, _) = placed(long.place(&keyed_heavy(key, 142_000), &snap, &mine, &mut rng));
        assert_eq!(chosen, slot("long01", 1));
    }

    #[test]
    fn heavy_pin_holds_when_backlog_below_cold_prefill() {
        // A warm pin with a small backlog stays, although its score is far
        // outside the affinity bound of the idle replica: 100K of waiting is
        // cheaper than a cold 142K prefill.
        let secret = [8u8; 32];
        let long = Placer::new(secret, Tier::Long);
        let mut rng = StdRng::seed_from_u64(1);
        let (snap, key) = long_pinned_pair(secret, 100_000, 0);
        let (chosen, record, _) = placed(long.place(
            &keyed_heavy(key.clone(), 142_000),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("long01", 0));
        assert_eq!(record.selection, Some("pinned"));
        assert!(record.chosen_score.unwrap() > record.best_score.unwrap() + 1.0);

        // The bound is inclusive: pinned load equal to the other's load plus
        // the prompt still holds; one token more moves.
        let (snap, key) = long_pinned_pair(secret, 242_000, 100_000);
        let (chosen, _, _) = placed(long.place(
            &keyed_heavy(key.clone(), 142_000),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("long01", 0));
        let (snap, key) = long_pinned_pair(secret, 242_001, 100_000);
        let (chosen, _, _) =
            placed(long.place(&keyed_heavy(key, 142_000), &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("long01", 1));
    }

    /// Two base replicas, `gpu01#0` pinned for the returned key with
    /// `pinned_backlog`, `gpu02#0` with `other_backlog`. The key's HRW home is
    /// the other slot, so only the pin rule can keep the request on the pin.
    fn base_pinned_pair(
        secret: [u8; 32],
        pinned_backlog: u64,
        other_backlog: u64,
    ) -> (Snapshot, AffinityKey) {
        let views = vec![
            with_backlog("gpu01", 0, pinned_backlog),
            with_backlog("gpu02", 0, other_backlog),
        ];
        let all: Vec<SlotId> = views.iter().map(|v| v.slot.clone()).collect();
        let key = find_key_with_home(&all, &slot("gpu02", 0));
        let mut snap = snap_with(views);
        let pid = pin_id(Tier::Base, &key, &secret);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - 1_000,
        );
        (snap, key)
    }

    fn keyed_prompt(key: AffinityKey, prompt_tokens: u64) -> PlaceInput {
        let mut input = keyed(key);
        input.prompt_tokens = prompt_tokens;
        input
    }

    #[test]
    fn short_pin_holds_when_backlog_below_cold_prefill() {
        // The regression case: a 10K prompt pinned to a slot 6K busier than
        // the idle one. Its score is outside the affinity slack, but waiting
        // on 6K is cheaper than a cold 10K prefill, so the pin holds.
        let secret = [8u8; 32];
        let (snap, key) = base_pinned_pair(secret, 6_000, 0);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed_prompt(key, 10_000),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.selection, Some("pinned"));
        assert!(record.chosen_score.unwrap() > record.best_score.unwrap() + 0.25);
        assert_eq!(record.pin_outcome, "held");
        assert_eq!(record.pin_age_ms, Some(1_000));
        assert_eq!(record.pinned_load, Some(6_000));
        assert_eq!(record.best_other_load, Some(0));
        assert_eq!(record.key_source, "header");
    }

    #[test]
    fn short_pin_released_when_backlog_exceeds_cold_prefill() {
        let secret = [8u8; 32];
        let (snap, key) = base_pinned_pair(secret, 30_000, 2_000);
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, pin_write) = placed(Placer::new(secret, Tier::Base).place(
            &keyed_prompt(key, 500),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.pin_outcome, "released_load");
        assert_eq!(record.pinned_load, Some(30_000));
        assert_eq!(record.best_other_load, Some(2_000));
        let (_, rewritten) = pin_write.expect("the moved pin is rewritten");
        assert_eq!(rewritten, slot("gpu02", 0));
    }

    #[test]
    fn pin_hold_factor_scales_the_cold_prefill_cost() {
        // A 40K prompt, pinned load 45K above the other: released at 1.0
        // (45K > 40K), held at 1.25 (45K <= 50K).
        let secret = [8u8; 32];
        let (snap, key) = base_pinned_pair(secret, 45_000, 0);
        let handle = Arc::new(ArcSwap::from_pointee(Tuning::default()));
        let p = Placer::with_tuning(secret, Tier::Base, handle.clone());
        let mut rng = StdRng::seed_from_u64(1);
        let input = keyed_prompt(key, 40_000);
        let (chosen, record, _) = placed(p.place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.pin_outcome, "released_load");

        handle.store(Arc::new(Tuning {
            pin_hold_factor: 1.25,
            ..Tuning::default()
        }));
        let (chosen, record, _) = placed(p.place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, slot("gpu01", 0));
        assert_eq!(record.pin_outcome, "held");
    }

    #[test]
    fn pin_outcome_covers_none_stale_boot_and_non_admitted_pins() {
        let secret = [4u8; 32];
        let slots = vec![slot("gpu01", 0), slot("gpu02", 0)];
        let key = find_key_with_home(&slots, &slot("gpu02", 0));
        let pid = pin_id(Tier::Base, &key, &secret);
        let mut rng = StdRng::seed_from_u64(1);

        // No pin at all.
        let snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
        let (_, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key.clone()),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(record.pin_outcome, "none");
        assert_eq!(record.pin_age_ms, None);

        // A pin from a previous boot is ignored.
        let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
        snap.host_boots = HashMap::from([("gpu01".to_string(), "boot-b".to_string())]);
        std::sync::Arc::make_mut(&mut snap.pins).insert_on_boot(
            *pid.as_bytes(),
            slot("gpu01", 0),
            NOW - 1_000,
            Some("boot-a".to_string()),
        );
        let (chosen, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key.clone()),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.pin_outcome, "stale_boot");
        assert_eq!(record.pinned, None);

        // A pin to a slot that is not an admitted candidate falls to HRW.
        let mut snap = snap_with(vec![ready_view("gpu01", 0), ready_view("gpu02", 0)]);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            slot("gpu01", 3),
            NOW - 1_000,
        );
        let (chosen, record, _) = placed(Placer::new(secret, Tier::Base).place(
            &keyed(key),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(chosen, slot("gpu02", 0));
        assert_eq!(record.pin_outcome, "not_admitted");
        assert_eq!(record.pinned_load, None);
    }

    #[test]
    fn oversized_heavy_never_admitted_on_base() {
        // Base engines accept 1M context, but a prompt over
        // HEAVY_BASE_MAX_PROMPT is long-tier work: on an idle base Fleet with
        // lane room it finds no candidate and falls back to legacy.
        let snap = snap_with(eight_slots());
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            placer().place(
                &heavy(HEAVY_BASE_MAX_PROMPT + 1),
                &snap,
                &HashMap::new(),
                &mut rng,
            ),
            LegacyReason::LaneFull,
        );
        assert_eq!(record.excluded[4], (Rule::Lane, 8));
        assert_eq!((record.lane_size, record.lane_cap), (0, 2));

        let (_, record, _) = placed(placer().place(
            &heavy(HEAVY_BASE_MAX_PROMPT),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));

        // The long tier has no such limit.
        let long = Placer::new([1u8; 32], Tier::Long);
        let (_, record, _) = placed(long.place(
            &heavy(HEAVY_BASE_MAX_PROMPT + 1),
            &snap,
            &HashMap::new(),
            &mut rng,
        ));
        assert_eq!(record.strategy, Some("heavy_long"));
    }

    #[test]
    fn heavy_pin_over_backlog_cap_moves() {
        // The pinned member's load plus the prompt exceeds HEAVY_BACKLOG_CAP,
        // so stage 2 excludes it and the pin can't hold: it spills to a new
        // lane member (the lane has room) and the pin is rewritten.
        let secret = [8u8; 32];
        let (snap, key, pinned) = pinned_member_with_room(secret, HEAVY_BACKLOG_CAP - 50_000);
        let mut input = keyed(key);
        input.prefill_heavy = true;
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
        // re-admitted as a new member. Its load (0) is under every other
        // load plus the prompt, so the load-aware pin holds.
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
        input.prefill_heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let (chosen, record, _) =
            placed(Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng));
        assert_eq!(chosen, pinned);
        assert_eq!(record.selection, Some("pinned"));
        assert_eq!(record.strategy, Some("heavy_lane_admit"));
    }

    // --- Capacity fall-backs, tier-keyed pins and record fields ---

    /// A heavy request on a saturated 4-slot base Fleet: every replica is a
    /// member at the backlog cap, so a usable snapshot falls it back to legacy.
    fn saturated_base() -> Snapshot {
        snap_with(
            (0..4)
                .map(|r| with_backlog("gpu01", r, HEAVY_BACKLOG_CAP))
                .collect(),
        )
    }

    #[test]
    fn lane_full_record_names_tier_class_and_lane() {
        let mut input = heavy(100_000);
        input.priority = -1;
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            placer().place(&input, &saturated_base(), &HashMap::new(), &mut rng),
            LegacyReason::LaneFull,
        );
        assert_eq!(record.tier, Tier::Base);
        assert_eq!(record.class, Class::Heavy);
        assert_eq!(record.priority_band, "neg");
        assert_eq!((record.lane_size, record.lane_cap), (4, 1));
        assert_eq!(record.prompt_tokens, 100_000);
        assert_eq!(record.eligible, 0);
        assert_eq!(record.slot, None);

        let long = Placer::new([1u8; 32], Tier::Long);
        let snap = snap_with(vec![with_backlog("long01", 0, LONG_BACKLOG_CAP)]);
        let record = legacy_record(
            long.place(&input, &snap, &HashMap::new(), &mut rng),
            LegacyReason::LongFull,
        );
        assert_eq!(record.tier, Tier::Long);
    }

    #[test]
    fn placed_record_carries_request_numbers() {
        let mut input = base_input();
        input.prompt_tokens = 4_000;
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
    }

    #[test]
    fn state_less_snapshots_are_never_capacity_fallbacks() {
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
    fn disabled_wins_over_a_capacity_fallback() {
        let mut snap = saturated_base();
        let mut rng = StdRng::seed_from_u64(1);
        legacy_record(
            placer().place(&heavy(100_000), &snap, &HashMap::new(), &mut rng),
            LegacyReason::LaneFull,
        );

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
        input.prefill_heavy = true;
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
    fn lane_full_record_never_contains_key_material() {
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
        input.prefill_heavy = true;
        input.prompt_tokens = 100_000;
        let mut rng = StdRng::seed_from_u64(1);
        let record = legacy_record(
            Placer::new(secret, Tier::Base).place(&input, &snap, &HashMap::new(), &mut rng),
            LegacyReason::LaneFull,
        );
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
            input.model = "z-ai/glm-5.3-flash".to_string();
            let key_bytes = [42u8; 16];
            if has_affinity {
                input.affinity = Some(AffinityKey::from_bytes(key_bytes));
                input.affinity_source = AffinitySource::Header;
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
                    .any(|v| v.slot == slot && first_exclusion(v, &input, input.now_ms, &Tuning::default()).is_none());
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
            input.model = "z-ai/glm-5.3-flash".to_string();
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
