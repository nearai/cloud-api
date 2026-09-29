//! Warm-cache affinity: a bounded HRW ranked walk plus a follow pin.
//!
//! [`select`] is the crate's entry point: given an optional affinity key,
//! an optional follow pin, and the current eligible-slot scores, it decides
//! which replica slot to route to and whether a new pin should be written.
//! Slots are ranked by their [`SlotId::hrw_label`] (`host#replica`). There is
//! no I/O here — `PinTable` is an in-memory table the caller (Task 6) loads
//! from and persists to Valkey off the request path.
//!
//! Privacy: [`AffinityKey`] and [`PinId`] are derived from customer content
//! (the conversation/session identity), so neither implements `Debug`,
//! `Display`, nor `Serialize`. Never log them.

use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use rand::{Rng, RngExt};
use sha2::{Digest, Sha256};

use crate::consts::{AFFINITY_ABS_SLACK, AFFINITY_EPS, PIN_TTL_MS};
use crate::policy::Tier;
use crate::snapshot::SlotId;

/// An opaque, unlinkable-by-inspection affinity key (e.g. derived from a
/// conversation id). Intentionally has no `Debug`/`Display`/`Serialize`.
#[derive(Clone)]
pub struct AffinityKey([u8; 16]);

impl AffinityKey {
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    /// Lowercase hex encoding of the 16 key bytes, for the caller to carry
    /// the key through an opaque channel (e.g. `params.extra`) between the
    /// request path that derives it and the placement decision that reads
    /// it. This is an explicit, deliberate conversion — not a `Display`/
    /// `Debug`/`Serialize` impl — so it never fires from a `{:?}`/log call;
    /// callers must still never log the returned string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Inverse of [`Self::to_hex`]: parses a lowercase (or uppercase) hex
    /// string back into an `AffinityKey`. Returns `None` if `s` is not
    /// exactly 32 hex characters.
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex::decode(s).ok()?;
        let arr: [u8; 16] = bytes.try_into().ok()?;
        Some(Self(arr))
    }
}

/// An opaque follow-pin identifier: `HMAC-SHA256(pin_secret, key)`,
/// truncated to 16 bytes. Written to Valkey as hex via [`PinId::to_hex`].
/// Intentionally has no `Debug`/`Display`/`Serialize`.
pub struct PinId([u8; 16]);

impl PinId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

type HmacSha256 = Hmac<Sha256>;

/// Derive a [`PinId`] for `key` on `tier`: `HMAC(pin_secret, tier ‖ 0x00 ‖
/// key)`. The tier is in the MAC input so the base and long placers never
/// read or overwrite each other's pins, though both share one pins stream.
pub fn pin_id(tier: Tier, key: &AffinityKey, pin_secret: &[u8; 32]) -> PinId {
    // A 32-byte key is always valid for HMAC-SHA256; this never fails.
    let mut mac = HmacSha256::new_from_slice(pin_secret).expect("32-byte HMAC key is always valid");
    mac.update(tier.as_str().as_bytes());
    mac.update(&[0u8]);
    mac.update(&key.0);
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    PinId(out)
}

/// In-memory follow-pin table: pin id -> (slot, written-at ms). The caller
/// loads this from Valkey and persists writes back; this crate never talks
/// to Valkey directly.
///
/// `Clone` so a reader can copy-on-write it behind an `Arc` (see
/// `Snapshot::pins`).
#[derive(Clone, Default)]
pub struct PinTable {
    entries: HashMap<[u8; 16], (SlotId, u64)>,
}

impl PinTable {
    /// The pinned `(slot, written-at ms)`, if `id` has an entry that hasn't
    /// expired: valid while `now_ms < at_ms + PIN_TTL_MS` (saturating, so an
    /// `at_ms` from the far past or a clock skew never panics or wraps to
    /// "still valid").
    pub fn get(&self, id: &PinId, now_ms: u64) -> Option<(&SlotId, u64)> {
        let (slot, at_ms) = self.entries.get(&id.0)?;
        if now_ms < at_ms.saturating_add(PIN_TTL_MS) {
            Some((slot, *at_ms))
        } else {
            None
        }
    }

    /// Write (or overwrite) the pin for `id`.
    pub fn insert(&mut self, id: [u8; 16], slot: SlotId, at_ms: u64) {
        self.entries.insert(id, (slot, at_ms));
    }

    /// Drop every pin to a slot `keep` rejects (e.g. a replica index that is
    /// gone from its host's latest frame). A pin to a missing slot would fall
    /// through to HRW anyway; this keeps the table from carrying dead slots.
    pub fn retain_slots(&mut self, mut keep: impl FnMut(&SlotId) -> bool) {
        self.entries.retain(|_, (slot, _)| keep(slot));
    }

    /// Drop every pin to a slot on `host`: after the host reboots (see
    /// `snapshot::Ingest::take_rebooted_hosts`) its replicas' prefix caches
    /// are cold, so a pin there no longer buys a warm prefill.
    pub fn drop_host(&mut self, host: &str) {
        self.retain_slots(|slot| slot.host != host);
    }

    /// Drop every entry [`Self::get`] would already treat as expired at
    /// `now_ms`, so a long-lived table stays bounded.
    pub fn prune(&mut self, now_ms: u64) {
        self.entries
            .retain(|_, (_, at_ms)| now_ms < at_ms.saturating_add(PIN_TTL_MS));
    }

    /// Number of entries, expired or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Rendezvous-hash (HRW) rank of `slots` for `key`: descending by
/// `sha256(key ‖ slot.hrw_label())[..8]` as a big-endian `u64`.
/// Deterministic for a given `(key, slots)` pair, and stable under removal —
/// dropping a slot from the input never reorders the others (classic HRW
/// property, since each slot's score depends only on `key` and itself).
pub fn hrw_rank(key: &AffinityKey, slots: &[&SlotId]) -> Vec<SlotId> {
    let mut scored: Vec<(u64, &SlotId)> = slots
        .iter()
        .map(|slot| {
            let mut hasher = Sha256::new();
            hasher.update(key.0);
            hasher.update(slot.hrw_label().as_bytes());
            let digest = hasher.finalize();
            let val = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
            (val, *slot)
        })
        .collect();
    // Break ties (astronomically unlikely for real sha256 output, but keeps
    // the ordering total and deterministic) by slot.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, s)| s.clone()).collect()
}

/// A selection outcome. `rank` in `Spill` is 1-based position in the HRW
/// walk (`Home` is implicitly rank 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The caller's follow pin was honored.
    Pinned,
    /// The key's top-ranked (HRW rank 1) host was chosen.
    Home,
    /// A lower-ranked host was chosen because Home (and any host ranked
    /// before it) was over the affinity bound.
    Spill { rank: u8 },
    /// No affinity key: chose the lower-scoring of two uniformly sampled
    /// slots.
    BestOfTwo,
}

impl Selection {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Selection::Pinned => "pinned",
            Selection::Home => "home",
            Selection::Spill { .. } => "spill",
            Selection::BestOfTwo => "best_of_two",
        }
    }
}

/// The result of [`select`].
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub slot: SlotId,
    pub selection: Selection,
    /// The key's HRW rank-1 slot, for keyed selections (`Pinned`, `Home`,
    /// `Spill`). `None` for keyless `BestOfTwo`.
    pub home: Option<SlotId>,
    /// Whether the caller should (re)write the follow pin: true when the
    /// chosen slot differs from home (a spill), or when an existing pin
    /// moved elsewhere (including being dropped back to home).
    pub write_pin: bool,
}

/// The bounded-load affinity test: is `score` close enough to the fleet's
/// `best` score to keep routing there?
///
/// `bound = max(best * (1 + EPS), best + ABS_SLACK)` — the absolute slack
/// keeps affinity intact when the whole fleet is near idle and `best` is
/// close to 0, where a purely relative bound would collapse to ~0 and
/// reject any nonzero score.
fn within_bound(score: f64, best: f64) -> bool {
    let bound = f64::max(best * (1.0 + AFFINITY_EPS), best + AFFINITY_ABS_SLACK);
    score <= bound
}

/// Decide which slot to route to. `scores` holds eligible slots only (the
/// caller has already applied the rules); returns `None` when `scores` is
/// empty, so the caller falls back to the legacy `Fleet::acquire_index`
/// path. See the module doc and `Selection` for the decision rules.
///
/// A pin to a slot that is not in `scores` (ineligible, or gone from its
/// host's frame) is ignored and the keyed HRW walk decides.
///
/// `pin_ignores_bound` makes a pin to any slot in `scores` win whatever its
/// score. The placer sets it for a prompt-heavy request whose pin passed its
/// load test instead (`decision::heavy_pin_holds`: stay unless waiting on
/// the pin costs more than a cold prefill elsewhere); a pin that fails that
/// test is left out of `scores` altogether.
pub fn select(
    key: Option<&AffinityKey>,
    pin: Option<&SlotId>,
    pin_ignores_bound: bool,
    scores: &[(SlotId, f64)],
    rng: &mut impl Rng,
) -> Option<Selected> {
    if scores.is_empty() {
        return None;
    }
    let best = scores.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);
    // Built once so the HRW walk below (and the pin/fallback lookups) are
    // O(slots) instead of a linear scan per lookup.
    let by_slot: HashMap<&SlotId, f64> = scores.iter().map(|(h, s)| (h, *s)).collect();
    let score_of = |slot: &SlotId| by_slot.get(slot).copied();

    // The HRW rank, only computed when a key is present. Keyed selections
    // (Pinned/Home/Spill) always report `home = Some(rank[0])`; keyless
    // BestOfTwo reports `None`.
    let rank: Option<Vec<SlotId>> = key.map(|k| {
        let slots: Vec<&SlotId> = scores.iter().map(|(h, _)| h).collect();
        hrw_rank(k, &slots)
    });
    let home = rank.as_ref().map(|r| r[0].clone());

    // Rule 1: an in-bound pin (or one the caller vouched for with
    // `pin_ignores_bound`) wins outright.
    if let Some(pin_slot) = pin {
        if let Some(score) = score_of(pin_slot) {
            if pin_ignores_bound || within_bound(score, best) {
                return Some(Selected {
                    slot: pin_slot.clone(),
                    selection: Selection::Pinned,
                    home: home.clone(),
                    write_pin: false,
                });
            }
        }
    }

    // Rule 2: keyed HRW walk — first slot in rank within bound.
    if let Some(rank) = rank {
        let chosen_idx = rank
            .iter()
            .position(|h| score_of(h).is_some_and(|s| within_bound(s, best)))
            .unwrap_or_else(|| {
                // Unreachable in practice: the best-scoring slot is always
                // within its own bound. Kept as a no-panic fallback to the
                // min-score slot, per the crate's fail-open posture.
                rank.iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let sa = score_of(a).unwrap_or(f64::INFINITY);
                        let sb = score_of(b).unwrap_or(f64::INFINITY);
                        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let chosen = rank[chosen_idx].clone();
        let selection = if chosen_idx == 0 {
            Selection::Home
        } else {
            Selection::Spill {
                rank: u8::try_from(chosen_idx + 1).unwrap_or(u8::MAX),
            }
        };
        let write_pin = chosen != rank[0] || pin.is_some();
        return Some(Selected {
            slot: chosen,
            selection,
            home: Some(rank[0].clone()),
            write_pin,
        });
    }

    // Rule 3: no key — sample 2 distinct indices uniformly and take the
    // lower score (first-sampled wins ties). A single slot is taken as-is.
    if scores.len() == 1 {
        return Some(Selected {
            slot: scores[0].0.clone(),
            selection: Selection::BestOfTwo,
            home: None,
            write_pin: false,
        });
    }
    let n = scores.len();
    let i = rng.random_range(0..n);
    let mut j = rng.random_range(0..n);
    while j == i {
        j = rng.random_range(0..n);
    }
    let chosen = if scores[i].1 <= scores[j].1 { i } else { j };
    Some(Selected {
        slot: scores[chosen].0.clone(),
        selection: Selection::BestOfTwo,
        home: None,
        write_pin: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Tier;
    use crate::testkit::slot;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashMap;

    /// Slot `host#0`: most selection tests only need one replica per host.
    fn s(host: &str) -> SlotId {
        slot(host, 0)
    }

    fn find_key_with_rank(slots: &[SlotId], want: &[SlotId]) -> AffinityKey {
        let refs: Vec<&SlotId> = slots.iter().collect();
        for seed in 0u128.. {
            let bytes = seed.to_be_bytes();
            let key = AffinityKey::from_bytes(bytes);
            let rank = hrw_rank(&key, &refs);
            if rank.len() >= want.len() && rank.iter().take(want.len()).eq(want.iter()) {
                return key;
            }
        }
        unreachable!("no key found within u128 search space")
    }

    #[test]
    fn drop_host_removes_only_that_hosts_pins() {
        let mut t = PinTable::default();
        let slot = |host: &str, replica| SlotId {
            host: host.into(),
            replica,
        };
        t.insert([1u8; 16], slot("gpu01", 0), 0);
        t.insert([2u8; 16], slot("gpu01", 1), 0);
        t.insert([3u8; 16], slot("gpu02", 0), 0);
        t.drop_host("gpu01");
        assert_eq!(t.len(), 1);
        assert!(t.entries.contains_key(&[3u8; 16]));
    }

    #[test]
    fn empty_scores_is_none() {
        let mut rng = StdRng::seed_from_u64(1);
        let scores: Vec<(SlotId, f64)> = vec![];
        assert!(select(None, None, false, &scores, &mut rng).is_none());
    }

    #[test]
    fn home_when_within_bound() {
        let key = find_key_with_rank(&[s("gpu02"), s("gpu08")], &[s("gpu02")]);
        let scores = vec![(s("gpu02"), 0.30), (s("gpu08"), 0.28)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), None, false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu02"));
        assert_eq!(sel.selection, Selection::Home);
        assert!(!sel.write_pin);
        assert_eq!(sel.home, Some(s("gpu02")));
    }

    #[test]
    fn idle_fleet_keeps_home() {
        // best (gpu-a) is 0.0; home (gpu-b) is 0.05. Bound = max(0*1.25,
        // 0+1.0) = 1.0, so the absolute slack keeps affinity even though the
        // fleet is near idle and a relative-only bound would collapse to ~0.
        let key = find_key_with_rank(&[s("gpu-a"), s("gpu-b")], &[s("gpu-b")]);
        let scores = vec![(s("gpu-a"), 0.0), (s("gpu-b"), 0.05)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), None, false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu-b"));
        assert_eq!(sel.selection, Selection::Home);
    }

    #[test]
    fn jitter_within_one_unit_keeps_home() {
        // gpu03 snapshot B: r0 runs 8 streams at 455.2 tok/s, r1 12 at 469.4,
        // both max_running 32 and no queue. A 4-stream difference is ordinary
        // jitter; a conversation homed on the busier r1 must stay there.
        use crate::score::{fleet_median_tps, replica_score, Pending};
        let mut r0 = crate::testkit::view("gpu03", 0);
        r0.state.limits.max_running = Some(32);
        r0.state.load.running = Some(8);
        r0.state.load.gen_tps = Some(455.2);
        let mut r1 = crate::testkit::view("gpu03", 1);
        r1.state.limits.max_running = Some(32);
        r1.state.load.running = Some(12);
        r1.state.load.gen_tps = Some(469.4);
        let median = fleet_median_tps(&[&r0, &r1]);
        let scores = vec![
            (
                r0.slot.clone(),
                replica_score(&r0, Pending::default(), median),
            ),
            (
                r1.slot.clone(),
                replica_score(&r1, Pending::default(), median),
            ),
        ];
        assert!(scores[1].1 > scores[0].1, "r1 is the busier replica");

        let key = find_key_with_rank(&[r0.slot.clone(), r1.slot.clone()], &[r1.slot.clone()]);
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), None, false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, r1.slot);
        assert_eq!(sel.selection, Selection::Home);
        assert!(!sel.write_pin);
    }

    #[test]
    fn spill_then_follow_pin() {
        let key = find_key_with_rank(&[s("gpu02"), s("gpu08")], &[s("gpu02"), s("gpu08")]);
        let mut rng = StdRng::seed_from_u64(1);

        // Turn 1: home (gpu02) is overloaded at 2.0; gpu08 at 0.4 is the
        // next slot in rank and within bound (best 0.4, bound 0.4+1.0 = 1.4).
        let scores = vec![(s("gpu02"), 2.0), (s("gpu08"), 0.4)];
        let sel = select(Some(&key), None, false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu08"));
        assert_eq!(sel.selection, Selection::Spill { rank: 2 });
        assert!(sel.write_pin, "spill away from home must write a pin");
        assert_eq!(sel.home, Some(s("gpu02")));

        // Turn 2: caller now passes the written pin; gpu02 is still hot.
        let sel = select(Some(&key), Some(&s("gpu08")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu08"));
        assert_eq!(sel.selection, Selection::Pinned);
        assert!(!sel.write_pin);

        // Turn 3: gpu02 cools to 0.35, gpu08 drifts to 0.42. best = 0.35,
        // bound = max(0.35*1.25=0.4375, 0.35+1.0=1.35) = 1.35; 0.42 <= 1.35
        // so the pin holds (no flap).
        let scores = vec![(s("gpu02"), 0.35), (s("gpu08"), 0.42)];
        let sel = select(Some(&key), Some(&s("gpu08")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu08"));
        assert_eq!(sel.selection, Selection::Pinned);
        assert!(!sel.write_pin);

        // Turn 4: gpu08 spikes to 2.0, past the bound (1.35) — pin is
        // ignored and the walk returns to home (gpu02), moving the pin.
        // (Was 0.9 against a 0.45 bound; AFFINITY_ABS_SLACK is now 1.0, so
        // the spike must clear a full unit to leave the pin.)
        let scores = vec![(s("gpu02"), 0.35), (s("gpu08"), 2.0)];
        let sel = select(Some(&key), Some(&s("gpu08")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu02"));
        assert_eq!(sel.selection, Selection::Home);
        assert!(sel.write_pin, "pin moving back to home must write a pin");
    }

    #[test]
    fn pin_ignoring_bound_holds_outside_bound() {
        let key = find_key_with_rank(&[s("gpu02"), s("gpu08")], &[s("gpu02")]);
        let scores = vec![(s("gpu02"), 0.1), (s("gpu08"), 6.0)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), Some(&s("gpu08")), true, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu08"));
        assert_eq!(sel.selection, Selection::Pinned);
        assert!(!sel.write_pin);
        // Without the flag, the same pin is out of bound and ignored.
        let sel = select(Some(&key), Some(&s("gpu08")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.selection, Selection::Home);
        // With the flag, a pin to a slot not in `scores` still falls through.
        let sel = select(Some(&key), Some(&s("gpu05")), true, &scores, &mut rng).unwrap();
        assert_eq!(sel.selection, Selection::Home);
    }

    #[test]
    fn pin_to_ineligible_slot_is_ignored() {
        // The pin names a slot that isn't in `scores` at all (e.g. it fell
        // out of eligibility); the keyed walk decides instead.
        let key = find_key_with_rank(&[s("gpu02"), s("gpu08")], &[s("gpu02")]);
        let scores = vec![(s("gpu02"), 0.2), (s("gpu08"), 0.9)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), Some(&s("gpu05")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("gpu02"));
        assert_eq!(sel.selection, Selection::Home);
        assert!(
            sel.write_pin,
            "an ignored pin (naming a now-ineligible slot) must write a fresh pin at home"
        );
    }

    #[test]
    fn pin_to_other_replica_on_same_host_is_a_different_slot() {
        // A pin to gpu02#1 is not satisfied by gpu02#0.
        let key = find_key_with_rank(&[slot("gpu02", 0)], &[slot("gpu02", 0)]);
        let scores = vec![(slot("gpu02", 0), 0.1)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(
            Some(&key),
            Some(&slot("gpu02", 1)),
            false,
            &scores,
            &mut rng,
        )
        .unwrap();
        assert_eq!(sel.slot, slot("gpu02", 0));
        assert_eq!(sel.selection, Selection::Home);
        assert!(sel.write_pin);
    }

    #[test]
    fn pin_for_unknown_slot_ignored() {
        // E19: pin set but no key and the pinned slot isn't eligible either
        // — falls all the way through to keyless BestOfTwo.
        let scores = vec![(s("hostA"), 0.1), (s("hostB"), 0.2)];
        let mut rng = StdRng::seed_from_u64(7);
        let sel = select(None, Some(&s("ghost")), false, &scores, &mut rng).unwrap();
        assert_eq!(sel.selection, Selection::BestOfTwo);
        assert_eq!(sel.slot, s("hostA"));
        assert_eq!(sel.home, None);
        assert!(!sel.write_pin);
    }

    #[test]
    fn keyless_uses_best_of_two() {
        // With exactly two eligible slots, BestOfTwo always samples both,
        // so the lower-scoring one is deterministic regardless of seed.
        let scores = vec![(s("hostA"), 1.0), (s("hostB"), 2.0)];
        for seed in 0..10u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let sel = select(None, None, false, &scores, &mut rng).unwrap();
            assert_eq!(sel.slot, s("hostA"));
            assert_eq!(sel.selection, Selection::BestOfTwo);
            assert_eq!(sel.home, None);
            assert!(!sel.write_pin);
        }
    }

    #[test]
    fn keyless_best_of_two_with_four_slots_is_not_always_global_min() {
        // With 4+ slots, BestOfTwo samples only 2 of them, so the pick is
        // the lower-scored of the *sampled* pair — not necessarily the
        // fleet-wide minimum. Assert both properties across seeds: every
        // pick is one of the eligible slots, and at least one seed picks
        // something other than the global min ("hostA").
        let scores = vec![
            (s("hostA"), 0.1), // global min
            (s("hostB"), 0.5),
            (s("hostC"), 0.6),
            (s("hostD"), 0.7),
        ];
        let by_slot: HashMap<&SlotId, f64> = scores.iter().map(|(h, s)| (h, *s)).collect();

        let mut saw_non_global_min = false;
        for seed in 0..50u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let sel = select(None, None, false, &scores, &mut rng).unwrap();
            assert_eq!(sel.selection, Selection::BestOfTwo);
            assert!(
                by_slot.contains_key(&sel.slot),
                "picked slot must be one of the eligible slots"
            );
            if sel.slot != s("hostA") {
                saw_non_global_min = true;
            }
        }
        assert!(
            saw_non_global_min,
            "with 4 slots, best-of-two should sometimes miss the global min"
        );
    }

    #[test]
    fn hrw_rank_is_input_order_independent() {
        let key = AffinityKey::from_bytes([11u8; 16]);
        let slots = [slot("gpu01", 0), slot("gpu01", 1), s("gpu03"), s("gpu04")];
        let refs: Vec<&SlotId> = slots.iter().collect();
        let mut reversed = refs.clone();
        reversed.reverse();
        assert_eq!(hrw_rank(&key, &refs), hrw_rank(&key, &reversed));
    }

    #[test]
    fn keyless_single_slot_is_taken() {
        let scores = vec![(s("hostA"), 1.0)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(None, None, false, &scores, &mut rng).unwrap();
        assert_eq!(sel.slot, s("hostA"));
        assert_eq!(sel.selection, Selection::BestOfTwo);
    }

    #[test]
    fn hrw_rank_is_deterministic_and_stable_under_removal() {
        let key = AffinityKey::from_bytes([7u8; 16]);
        let slots = [slot("gpu01", 0), slot("gpu01", 1), s("gpu02"), s("gpu03")];
        let refs: Vec<&SlotId> = slots.iter().collect();
        let full = hrw_rank(&key, &refs);
        assert_eq!(full, hrw_rank(&key, &refs), "deterministic for same input");

        // Remove one slot (not necessarily the top-ranked) and check the
        // relative order of the rest is preserved.
        let removed = full[full.len() / 2].clone();
        let remaining: Vec<&SlotId> = refs.iter().copied().filter(|h| **h != removed).collect();
        let reduced = hrw_rank(&key, &remaining);
        let expected: Vec<SlotId> = full.into_iter().filter(|h| *h != removed).collect();
        assert_eq!(reduced, expected);
    }

    #[test]
    fn every_selection_has_a_static_tag() {
        let tags = [
            Selection::Pinned.as_str(),
            Selection::Home.as_str(),
            Selection::Spill { rank: 3 }.as_str(),
            Selection::BestOfTwo.as_str(),
        ];
        assert_eq!(tags, ["pinned", "home", "spill", "best_of_two"]);
    }

    #[test]
    fn affinity_key_hex_round_trips() {
        let key = AffinityKey::from_bytes([0xabu8; 16]);
        let hex = key.to_hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(hex, "ab".repeat(16));
        let decoded = AffinityKey::from_hex(&hex).expect("valid hex decodes");
        assert_eq!(decoded.to_hex(), hex);
    }

    #[test]
    fn affinity_key_from_hex_rejects_wrong_length_and_garbage() {
        assert!(AffinityKey::from_hex("abcd").is_none());
        assert!(AffinityKey::from_hex("not-hex-at-all-not-hex-at-all!!").is_none());
    }

    #[test]
    fn pin_id_is_deterministic_and_key_dependent() {
        let secret = [9u8; 32];
        let key1 = AffinityKey::from_bytes([1u8; 16]);
        let key2 = AffinityKey::from_bytes([2u8; 16]);
        let id1 = pin_id(Tier::Base, &key1, &secret);
        let id1_again = pin_id(Tier::Base, &key1, &secret);
        let id2 = pin_id(Tier::Base, &key2, &secret);
        assert_eq!(id1.as_bytes(), id1_again.as_bytes());
        assert_ne!(id1.as_bytes(), id2.as_bytes());
        assert_eq!(id1.to_hex().len(), 32);
    }

    #[test]
    fn pin_id_is_keyed_by_tier() {
        let secret = [9u8; 32];
        let key = AffinityKey::from_bytes([1u8; 16]);
        let base = pin_id(Tier::Base, &key, &secret);
        let long = pin_id(Tier::Long, &key, &secret);
        assert_ne!(base.as_bytes(), long.as_bytes());
        // HMAC(secret, tier || 0x00 || key), truncated to 16 bytes.
        let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
        mac.update(b"long\x00");
        mac.update(&[1u8; 16]);
        assert_eq!(long.as_bytes()[..], mac.finalize().into_bytes()[..16]);
    }

    #[test]
    fn pin_table_expires_by_ttl() {
        let mut table = PinTable::default();
        let id = pin_id(Tier::Base, &AffinityKey::from_bytes([3u8; 16]), &[1u8; 32]);
        table.insert(*id.as_bytes(), slot("gpu09", 1), 1_000);
        assert_eq!(table.get(&id, 1_000), Some((&slot("gpu09", 1), 1_000)));
        assert_eq!(
            table.get(&id, 1_000 + PIN_TTL_MS - 1),
            Some((&slot("gpu09", 1), 1_000))
        );
        assert_eq!(table.get(&id, 1_000 + PIN_TTL_MS), None);
    }

    #[test]
    fn pin_table_prune_drops_only_expired() {
        let mut table = PinTable::default();
        table.insert([1u8; 16], s("old"), 1_000);
        table.insert([2u8; 16], s("new"), 5_000);
        table.prune(1_000 + PIN_TTL_MS);
        assert_eq!(table.len(), 1);
        let new_id = PinId([2u8; 16]);
        assert_eq!(
            table.get(&new_id, 1_000 + PIN_TTL_MS),
            Some((&s("new"), 5_000))
        );
    }

    #[test]
    fn pin_table_retain_slots_drops_pins_to_removed_replicas() {
        let mut table = PinTable::default();
        table.insert([1u8; 16], slot("gpu01", 0), 1_000);
        table.insert([2u8; 16], slot("gpu01", 3), 1_000);
        table.retain_slots(|slot| slot.replica < 2);
        assert_eq!(table.len(), 1);
        assert!(table.get(&PinId([2u8; 16]), 1_000).is_none());
        assert!(table.get(&PinId([1u8; 16]), 1_000).is_some());
    }
}
