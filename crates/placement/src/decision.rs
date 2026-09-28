//! The `Placer` facade: ties `rules.rs`, `score.rs`, and `affinity.rs`
//! together into a single placement decision, plus the `DecisionRecord` the
//! caller logs (IDs and numbers only — see the crate's privacy rules).

use std::collections::HashMap;

use rand::Rng;

use crate::affinity::{pin_id, select, AffinityKey, PinId, Selection};
use crate::consts::{COVERED_MODELS, FRESH_MAX_MS, HOST_REPLICA, PIN_TTL_MS};
use crate::rules::{first_exclusion, Rule, RULES};
use crate::score::{fleet_median_tps, host_score, pending_for, split_pending, Pending};
use crate::snapshot::{ReplicaView, Snapshot};

/// Per-request inputs the eligibility rules (`rules.rs`) check a
/// [`crate::snapshot::ReplicaView`] against, plus the rest of what
/// `Placer::place` needs to score and pick a host.
///
/// `affinity` holds an [`AffinityKey`], which has no `Debug`/`Display`, so
/// `PlaceInput` implements `Debug` manually and redacts it (see the manual
/// `impl` below) rather than deriving it.
#[derive(Clone)]
pub struct PlaceInput {
    pub model: String,
    pub prompt_tokens_est: u64,
    /// The caller-derived affinity key (e.g. from a conversation id), if
    /// this request carries one. Never logged — see `affinity.rs`.
    pub affinity: Option<AffinityKey>,
    pub affinity_source: AffinitySource,
    /// Host ids known (from Fleet's existing tier knowledge) to serve long
    /// context, passed in by the caller.
    pub long_context_hosts: Vec<String>,
    pub now_ms: u64,
}

impl std::fmt::Debug for PlaceInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceInput")
            .field("model", &self.model)
            .field("prompt_tokens_est", &self.prompt_tokens_est)
            .field("affinity", &self.affinity.is_some())
            .field("affinity_source", &self.affinity_source)
            .field("long_context_hosts", &self.long_context_hosts)
            .field("now_ms", &self.now_ms)
            .finish()
    }
}

/// Where `PlaceInput::affinity` came from, for the (content-free)
/// `DecisionRecord::affinity` field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AffinitySource {
    Client,
    Prefix,
    None,
}

impl AffinitySource {
    fn as_str(self) -> &'static str {
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
    pub fn as_str(self) -> &'static str {
        match self {
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
pub enum Decision {
    Place {
        host: String,
        record: DecisionRecord,
        pin_write: Option<(PinId, String)>,
    },
    Legacy {
        reason: LegacyReason,
        record: DecisionRecord,
    },
}

/// A content-free record of a placement decision, safe to log as-is (IDs
/// and numbers only). Must never carry `AffinityKey` or `PinId` bytes/hex.
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    pub outcome: &'static str,
    pub reason: Option<&'static str>,
    pub rank: Option<u8>,
    pub affinity: &'static str,
    pub selection: Option<&'static str>,
    pub host: Option<String>,
    pub home: Option<String>,
    pub pinned: Option<String>,
    /// Number of eligible **hosts** the decision scored over (not the
    /// number of eligible replicas — a host with 2 eligible replicas still
    /// counts once here).
    pub eligible: u16,
    pub excluded: [(Rule, u16); 5],
    pub chosen_score: Option<f64>,
    pub home_score: Option<f64>,
    pub best_score: Option<f64>,
    pub snapshot_age_ms: u64,
    pub pending_req: u32,
    pub chosen_backlog_tokens: Option<u64>,
}

impl DecisionRecord {
    fn legacy(input: &PlaceInput, snap: &Snapshot, reason: LegacyReason) -> Self {
        DecisionRecord {
            outcome: "legacy",
            reason: Some(reason.as_str()),
            rank: None,
            affinity: input.affinity_source.as_str(),
            selection: None,
            host: None,
            home: None,
            pinned: None,
            eligible: 0,
            excluded: RULES.map(|r| (r, 0)),
            chosen_score: None,
            home_score: None,
            best_score: None,
            snapshot_age_ms: input.now_ms.saturating_sub(snap.built_ms),
            pending_req: 0,
            chosen_backlog_tokens: None,
        }
    }
}

/// Sums the eligible replicas' prefill backlog on `views`, or `None` if none
/// of them report one.
fn sum_backlog(views: &[&ReplicaView]) -> Option<u64> {
    let mut total: Option<u64> = None;
    for v in views {
        if let Some(b) = v.report.load.prefill_backlog_tokens {
            total = Some(total.unwrap_or(0) + b);
        }
    }
    total
}

/// Builds a `Decision::Legacy` for `reason`, optionally overriding the
/// per-`Rule` exclusion tally (used once eligibility has already been
/// computed, e.g. for `NoneEligible`).
fn legacy(
    input: &PlaceInput,
    snap: &Snapshot,
    reason: LegacyReason,
    excluded: Option<[(Rule, u16); 5]>,
) -> Decision {
    let mut record = DecisionRecord::legacy(input, snap, reason);
    if let Some(excluded) = excluded {
        record.excluded = excluded;
    }
    Decision::Legacy { reason, record }
}

/// Per-host score (lower is better), plus the host-level `Pending` used to
/// compute it, for every host with at least one eligible replica.
fn score_hosts(
    eligible_by_host: &HashMap<String, Vec<&ReplicaView>>,
    snap: &Snapshot,
    mine: &HashMap<String, Pending>,
    median: f64,
) -> (Vec<(String, f64)>, HashMap<String, Pending>) {
    let mut host_scores: Vec<(String, f64)> = Vec::with_capacity(eligible_by_host.len());
    let mut host_pending: HashMap<String, Pending> = HashMap::new();
    for (host, views) in eligible_by_host {
        let n_eligible = views.len();
        let mine_pending = mine.get(host).copied().unwrap_or_default();
        let routed = snap.routed.get(&(host.clone(), HOST_REPLICA.to_string()));
        let pending = pending_for(routed, mine_pending);
        let split = split_pending(pending, n_eligible);
        let pairs: Vec<(&ReplicaView, Pending)> = views.iter().map(|v| (*v, split)).collect();
        let score = host_score(&pairs, median);
        host_scores.push((host.clone(), score));
        host_pending.insert(host.clone(), pending);
    }
    // `eligible_by_host` is a `HashMap`, so its iteration order (and thus
    // `host_scores`' order) is otherwise unspecified and can differ between
    // processes for the same input. `affinity::select`'s keyless BestOfTwo
    // path samples indices from `rng` against this order, so two `Placer`s
    // (or the same one on a re-run) must see the same order to draw the
    // same conclusion from the same rng seed — sort it deterministically.
    host_scores.sort_by(|a, b| a.0.cmp(&b.0));
    (host_scores, host_pending)
}

/// The pure placement decision-maker. Holds only the deployment's pin
/// secret — no I/O, no mutable state.
pub struct Placer {
    pin_secret: [u8; 32],
}

impl Placer {
    pub fn new(pin_secret: [u8; 32]) -> Self {
        Self { pin_secret }
    }

    pub fn place(
        &self,
        input: &PlaceInput,
        snap: &Snapshot,
        mine: &HashMap<String, Pending>,
        rng: &mut impl Rng,
    ) -> Decision {
        if !COVERED_MODELS.contains(&input.model.as_str()) {
            return legacy(input, snap, LegacyReason::NotCovered, None);
        }

        if snap.replicas.is_empty() {
            return legacy(input, snap, LegacyReason::NoState, None);
        }

        if input.now_ms.saturating_sub(snap.built_ms) > FRESH_MAX_MS {
            return legacy(input, snap, LegacyReason::Stale, None);
        }

        // Group eligible replicas by host, tallying exclusions in RULES
        // order for the record along the way.
        let mut eligible_by_host: HashMap<String, Vec<&ReplicaView>> = HashMap::new();
        let mut excluded_counts: HashMap<Rule, u16> = HashMap::new();
        for view in &snap.replicas {
            match first_exclusion(view, input, input.now_ms) {
                None => eligible_by_host
                    .entry(view.host_id.clone())
                    .or_default()
                    .push(view),
                Some(exclusion) => {
                    *excluded_counts.entry(exclusion.0).or_insert(0) += 1;
                }
            }
        }
        let excluded: [(Rule, u16); 5] = RULES.map(|r| (r, *excluded_counts.get(&r).unwrap_or(&0)));

        if eligible_by_host.is_empty() {
            return legacy(input, snap, LegacyReason::NoneEligible, Some(excluded));
        }

        let all_eligible: Vec<&ReplicaView> =
            eligible_by_host.values().flatten().copied().collect();
        let median = fleet_median_tps(&all_eligible);

        let (host_scores, host_pending) = score_hosts(&eligible_by_host, snap, mine, median);

        let best_score = host_scores
            .iter()
            .map(|(_, s)| *s)
            .fold(f64::INFINITY, f64::min);

        // Pin lookup: only when this request carries an affinity key.
        let pin_id_opt = input.affinity.as_ref().map(|k| pin_id(k, &self.pin_secret));
        let pin_lookup: Option<(String, u64)> = pin_id_opt.as_ref().and_then(|pid| {
            snap.pins
                .get(pid, input.now_ms)
                .map(|(host, at_ms)| (host.to_string(), at_ms))
        });

        let selected = match select(
            input.affinity.as_ref(),
            pin_lookup.as_ref().map(|(h, _)| h.as_str()),
            &host_scores,
            rng,
        ) {
            Some(s) => s,
            None => return legacy(input, snap, LegacyReason::NoneEligible, Some(excluded)),
        };

        let rank: Option<u8> = match selected.selection {
            Selection::Home => Some(1),
            Selection::Spill { rank } => Some(rank),
            Selection::Pinned | Selection::BestOfTwo => None,
        };
        let selection_str: &'static str = match selected.selection {
            Selection::Pinned => "pinned",
            Selection::Home => "home",
            Selection::Spill { .. } => "spill",
            Selection::BestOfTwo => "best_of_two",
        };

        let mut pin_write: Option<(PinId, String)> = None;
        if let Some(pid) = pin_id_opt {
            let should_write = match selected.selection {
                Selection::Pinned => pin_lookup
                    .as_ref()
                    .map(|(_, at_ms)| input.now_ms.saturating_sub(*at_ms) > PIN_TTL_MS / 2)
                    .unwrap_or(false),
                _ => selected.write_pin,
            };
            if should_write {
                pin_write = Some((pid, selected.host.clone()));
            }
        }

        let chosen_score = host_scores
            .iter()
            .find(|(h, _)| h == &selected.host)
            .map(|(_, s)| *s);
        let home_score = selected
            .home
            .as_ref()
            .and_then(|home| host_scores.iter().find(|(h, _)| h == home).map(|(_, s)| *s));
        let chosen_pending = host_pending
            .get(&selected.host)
            .copied()
            .unwrap_or_default();
        let chosen_backlog_tokens = eligible_by_host
            .get(&selected.host)
            .and_then(|views| sum_backlog(views));

        let record = DecisionRecord {
            outcome: "place",
            reason: None,
            rank,
            affinity: input.affinity_source.as_str(),
            selection: Some(selection_str),
            host: Some(selected.host.clone()),
            home: selected.home.clone(),
            pinned: pin_lookup.map(|(h, _)| h),
            eligible: eligible_by_host.len() as u16,
            excluded,
            chosen_score,
            home_score,
            best_score: Some(best_score),
            snapshot_age_ms: input.now_ms.saturating_sub(snap.built_ms),
            pending_req: chosen_pending.req,
            chosen_backlog_tokens,
        };

        Decision::Place {
            host: selected.host,
            record,
            pin_write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::AffinityKey;
    use crate::consts::{COVERED_MODELS, FRESH_MAX_MS, HOST_REPLICA, PIN_TTL_MS};
    use crate::snapshot::RoutedCounts;
    use crate::testkit::NOW;
    use ed25519_dalek::SigningKey;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    const MODEL: &str = "z-ai/glm-5.3-flash";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn base_input() -> PlaceInput {
        PlaceInput {
            model: MODEL.into(),
            prompt_tokens_est: 100,
            affinity: None,
            affinity_source: AffinitySource::None,
            long_context_hosts: Vec::new(),
            now_ms: NOW,
        }
    }

    fn ready_view(host: &str, replica: &str) -> ReplicaView {
        use crate::frame::{Lifecycle, Limits, Load, ReplicaReport};
        let pk = signing_key().verifying_key();
        ReplicaView {
            host_id: host.into(),
            replica_id: replica.into(),
            report: ReplicaReport {
                schema: 1,
                host_id: host.into(),
                replica_id: replica.into(),
                boot_id: "boot-a".into(),
                seq: 1,
                engine_sampled_at_ms: Some(NOW),
                reported_at_ms: NOW,
                lifecycle_state: Lifecycle::Ready,
                model: MODEL.into(),
                engine: "sglang".into(),
                engine_version: None,
                limits: Limits { max_running: None },
                load: Load {
                    running: Some(0),
                    queued: Some(0),
                    ..Load::default()
                },
                proxy_inflight: 0,
                report_key_id: crate::frame::key_id(&pk),
            },
        }
    }

    fn snap_with(views: Vec<ReplicaView>) -> Snapshot {
        Snapshot {
            built_ms: NOW,
            replicas: views,
            routed: HashMap::new(),
            pins: Default::default(),
        }
    }

    fn placer() -> Placer {
        Placer::new([1u8; 32])
    }

    #[test]
    fn caller_legacy_reasons_have_stable_names() {
        assert_eq!(LegacyReason::HostUnmapped.as_str(), "host_unmapped");
        assert_eq!(LegacyReason::KeyGroup.as_str(), "key_group");
        assert_eq!(LegacyReason::Incomplete.as_str(), "incomplete");
    }

    #[test]
    fn not_covered_is_legacy() {
        let mut input = base_input();
        input.model = "some-other-model".into();
        let snap = snap_with(vec![ready_view("gpu01", "r1")]);
        let mut rng = StdRng::seed_from_u64(1);
        let decision = placer().place(&input, &snap, &HashMap::new(), &mut rng);
        match decision {
            Decision::Legacy { reason, record } => {
                assert_eq!(reason, LegacyReason::NotCovered);
                assert_eq!(record.reason, Some("not_covered"));
                assert_eq!(record.outcome, "legacy");
            }
            Decision::Place { .. } => panic!("expected Legacy"),
        }
    }

    #[test]
    fn no_state_is_legacy() {
        let input = base_input();
        let snap = snap_with(vec![]);
        let mut rng = StdRng::seed_from_u64(1);
        let decision = placer().place(&input, &snap, &HashMap::new(), &mut rng);
        match decision {
            Decision::Legacy { reason, .. } => assert_eq!(reason, LegacyReason::NoState),
            Decision::Place { .. } => panic!("expected Legacy"),
        }
    }

    #[test]
    fn stale_snapshot_is_legacy() {
        let input = base_input();
        let mut snap = snap_with(vec![ready_view("gpu01", "r1")]);
        snap.built_ms = NOW - FRESH_MAX_MS - 1;
        let mut rng = StdRng::seed_from_u64(1);
        let decision = placer().place(&input, &snap, &HashMap::new(), &mut rng);
        match decision {
            Decision::Legacy { reason, record } => {
                assert_eq!(reason, LegacyReason::Stale);
                assert_eq!(record.reason, Some("stale"));
            }
            Decision::Place { .. } => panic!("expected Legacy"),
        }
    }

    #[test]
    fn none_eligible_is_legacy() {
        let input = base_input();
        let mut v = ready_view("gpu01", "r1");
        v.report.lifecycle_state = crate::frame::Lifecycle::Warming;
        let snap = snap_with(vec![v]);
        let mut rng = StdRng::seed_from_u64(1);
        let decision = placer().place(&input, &snap, &HashMap::new(), &mut rng);
        match decision {
            Decision::Legacy { reason, record } => {
                assert_eq!(reason, LegacyReason::NoneEligible);
                assert_eq!(record.reason, Some("none_eligible"));
                let (rule, count) = record.excluded[1];
                assert_eq!(rule, Rule::Lifecycle);
                assert_eq!(count, 1);
            }
            Decision::Place { .. } => panic!("expected Legacy"),
        }
    }

    #[test]
    fn two_placers_agree() {
        let key_bytes = [3u8; 16];
        let mut input = base_input();
        input.affinity = Some(AffinityKey::from_bytes(key_bytes));
        input.affinity_source = AffinitySource::Client;

        let snap = snap_with(vec![ready_view("gpu01", "r1"), ready_view("gpu02", "r1")]);

        let a = Placer::new([9u8; 32]);
        let b = Placer::new([9u8; 32]);

        let mut rng_a = StdRng::seed_from_u64(1);
        let mut rng_b = StdRng::seed_from_u64(2);

        let host_a = match a.place(&input, &snap, &HashMap::new(), &mut rng_a) {
            Decision::Place { host, .. } => host,
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        let host_b = match b.place(&input, &snap, &HashMap::new(), &mut rng_b) {
            Decision::Place { host, .. } => host,
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        assert_eq!(host_a, host_b);
    }

    /// Searches for an `AffinityKey` whose HRW rank over `hosts` puts `home`
    /// first, so a test can force a specific `Selection::Home`/`Spill`
    /// outcome instead of depending on whichever host an arbitrary key
    /// happens to rank first.
    fn find_key_with_home(hosts: &[&str], home: &str) -> AffinityKey {
        use crate::affinity::hrw_rank;
        for seed in 0u128.. {
            let key = AffinityKey::from_bytes(seed.to_be_bytes());
            if hrw_rank(&key, hosts).first().map(String::as_str) == Some(home) {
                return key;
            }
        }
        unreachable!("no key found within u128 search space")
    }

    #[test]
    fn two_placers_agree_after_pin() {
        let key = find_key_with_home(&["gpu01", "gpu02"], "gpu01");

        // gpu01 (home) is overloaded; gpu02 is the spill target.
        let mut hot = ready_view("gpu01", "r1");
        hot.report.load.running = Some(1000);
        let cold = ready_view("gpu02", "r1");
        let snap = snap_with(vec![hot, cold]);

        let secret = [5u8; 32];
        let a = Placer::new(secret);

        let mut input = base_input();
        input.affinity = Some(key.clone());
        input.affinity_source = AffinitySource::Client;

        let mut rng = StdRng::seed_from_u64(1);
        let (host_a, pin_write, record_a) = match a.place(&input, &snap, &HashMap::new(), &mut rng)
        {
            Decision::Place {
                host,
                pin_write,
                record,
            } => (host, pin_write, record),
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        assert_eq!(record_a.selection, Some("spill"));
        assert_eq!(record_a.home.as_deref(), Some("gpu01"));

        // Node A writes the pin into a fresh snapshot as node B would read it.
        let (pin_id_val, pinned_host) = pin_write.expect("spill away from home must write a pin");
        assert_eq!(pinned_host, host_a);
        let mut snap_b = snap_with(vec![
            {
                let mut hot = ready_view("gpu01", "r1");
                hot.report.load.running = Some(1000);
                hot
            },
            ready_view("gpu02", "r1"),
        ]);
        std::sync::Arc::make_mut(&mut snap_b.pins).insert(*pin_id_val.as_bytes(), pinned_host, NOW);

        let b = Placer::new(secret);
        let mut input_b = input.clone();
        input_b.affinity = Some(key);
        let mut rng_b = StdRng::seed_from_u64(2);
        match b.place(&input_b, &snap_b, &HashMap::new(), &mut rng_b) {
            Decision::Place { host, record, .. } => {
                assert_eq!(host, host_a);
                assert_eq!(record.selection, Some("pinned"));
            }
            Decision::Legacy { .. } => panic!("expected Place"),
        }
    }

    #[test]
    fn best_of_two_is_deterministic_across_replica_insertion_order() {
        // No affinity key, so `select` takes the keyless BestOfTwo path,
        // which samples indices against `host_scores`' order. Two snapshots
        // built from the same replicas in reverse insertion order must
        // still agree for the same rng seed, because `score_hosts` sorts
        // its output instead of relying on `HashMap` iteration order.
        let views_a = vec![
            ready_view("gpu01", "r1"),
            ready_view("gpu02", "r1"),
            ready_view("gpu03", "r1"),
            ready_view("gpu04", "r1"),
        ];
        let mut views_b = views_a.clone();
        views_b.reverse();

        let snap_a = snap_with(views_a);
        let snap_b = snap_with(views_b);
        let input = base_input();

        let mut rng_a = StdRng::seed_from_u64(99);
        let mut rng_b = StdRng::seed_from_u64(99);

        let host_a = match placer().place(&input, &snap_a, &HashMap::new(), &mut rng_a) {
            Decision::Place { host, .. } => host,
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        let host_b = match placer().place(&input, &snap_b, &HashMap::new(), &mut rng_b) {
            Decision::Place { host, .. } => host,
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        assert_eq!(host_a, host_b);
    }

    #[test]
    fn pinned_refreshes_after_half_ttl() {
        let key_bytes = [6u8; 16];
        let secret = [2u8; 32];
        let snap_views = vec![ready_view("gpu01", "r1"), ready_view("gpu02", "r1")];

        let mut input = base_input();
        input.affinity = Some(AffinityKey::from_bytes(key_bytes));
        input.affinity_source = AffinitySource::Client;

        let pid = pin_id(&AffinityKey::from_bytes(key_bytes), &secret);

        // Pin inserted just over half the TTL ago: must refresh.
        let mut snap_old = snap_with(snap_views.clone());
        std::sync::Arc::make_mut(&mut snap_old.pins).insert(
            *pid.as_bytes(),
            "gpu01".to_string(),
            NOW - (PIN_TTL_MS / 2 + 1),
        );
        let p = Placer::new(secret);
        let mut rng = StdRng::seed_from_u64(1);
        match p.place(&input, &snap_old, &HashMap::new(), &mut rng) {
            Decision::Place {
                record, pin_write, ..
            } => {
                assert_eq!(record.selection, Some("pinned"));
                assert!(pin_write.is_some(), "half-TTL-old pin should refresh");
            }
            Decision::Legacy { .. } => panic!("expected Place"),
        }

        // Pin inserted recently: no refresh needed.
        let mut snap_fresh = snap_with(snap_views);
        std::sync::Arc::make_mut(&mut snap_fresh.pins).insert(
            *pid.as_bytes(),
            "gpu01".to_string(),
            NOW - 1_000,
        );
        let mut rng2 = StdRng::seed_from_u64(1);
        match p.place(&input, &snap_fresh, &HashMap::new(), &mut rng2) {
            Decision::Place {
                record, pin_write, ..
            } => {
                assert_eq!(record.selection, Some("pinned"));
                assert!(pin_write.is_none(), "recent pin should not refresh");
            }
            Decision::Legacy { .. } => panic!("expected Place"),
        }
    }

    #[test]
    fn record_never_contains_key_material() {
        let key_bytes = [0xABu8; 16];
        let secret = [0xCDu8; 32];
        let pid = pin_id(&AffinityKey::from_bytes(key_bytes), &secret);

        let mut input = base_input();
        input.affinity = Some(AffinityKey::from_bytes(key_bytes));
        input.affinity_source = AffinitySource::Client;

        // Give the request a live pin so the `Pinned` path (and its
        // `pin_lookup`) actually runs, instead of trivially passing on a
        // Legacy or keyless decision that never touches key material.
        let mut snap = snap_with(vec![ready_view("gpu01", "r1"), ready_view("gpu02", "r1")]);
        std::sync::Arc::make_mut(&mut snap.pins).insert(
            *pid.as_bytes(),
            "gpu01".to_string(),
            NOW - 1_000,
        );

        let mut rng = StdRng::seed_from_u64(1);
        let decision = Placer::new(secret).place(&input, &snap, &HashMap::new(), &mut rng);
        let record = match decision {
            Decision::Place { record, .. } => {
                assert_eq!(record.selection, Some("pinned"));
                record
            }
            Decision::Legacy { .. } => panic!("expected Place"),
        };
        let debug_str = format!("{record:?}");

        assert!(
            !debug_str.contains(&pid.to_hex()),
            "record debug output leaked the pin id's hex encoding"
        );
        assert!(
            !debug_str.contains(&hex::encode(key_bytes)),
            "record debug output leaked the affinity key's hex encoding"
        );
        assert!(
            !debug_str.contains(&format!("{key_bytes:?}")),
            "record debug output leaked the affinity key's raw (decimal) bytes"
        );
        assert!(
            !debug_str.contains(&format!("{:?}", pid.as_bytes())),
            "record debug output leaked the pin id's raw (decimal) bytes"
        );

        // Belt-and-suspenders: a leaked pin id or raw key byte dump would
        // also show up as a run of 32+ hex characters.
        let mut run = 0usize;
        for c in debug_str.chars() {
            if c.is_ascii_hexdigit() {
                run += 1;
                assert!(
                    run < 32,
                    "found a 32+ hex-char run in record debug output: {debug_str}"
                );
            } else {
                run = 0;
            }
        }
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
        replica: &str,
        spec: (crate::frame::Lifecycle, bool, u32),
    ) -> ReplicaView {
        let (lifecycle, fresh, running) = spec;
        let mut v = ready_view(host, replica);
        v.report.lifecycle_state = lifecycle;
        v.report.engine_sampled_at_ms = if fresh {
            Some(NOW)
        } else {
            Some(NOW - FRESH_MAX_MS - 1)
        };
        v.report.load.running = Some(running);
        v
    }

    proptest! {
        /// Small random snapshots with an independently random lifecycle,
        /// freshness, and load per replica (not just per run), plus an
        /// optional affinity key, follow pin, and `mine` entry. Whenever the
        /// placer actually places, the chosen host must have at least one
        /// eligible replica.
        #[test]
        fn placed_host_is_always_eligible(
            specs in proptest::collection::vec(replica_spec_strategy(), 1..=12),
            has_affinity in proptest::bool::ANY,
            has_pin in proptest::bool::ANY,
            has_mine in proptest::bool::ANY,
        ) {
            let mut views = Vec::new();
            for (i, spec) in specs.into_iter().enumerate() {
                let host = format!("gpu{}", i / 2);
                let replica = format!("r{}", i % 2);
                views.push(view_from_spec(&host, &replica, spec));
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
                let pid = pin_id(&AffinityKey::from_bytes(key_bytes), &[1u8; 32]);
                std::sync::Arc::make_mut(&mut snap.pins).insert(*pid.as_bytes(), "gpu0".to_string(), NOW - 1_000);
            }

            let mut mine = HashMap::new();
            if has_mine {
                mine.insert("gpu0".to_string(), Pending { req: 2, tok: 500 });
            }

            let mut rng = StdRng::seed_from_u64(42);
            let decision = placer().place(&input, &snap, &mine, &mut rng);
            if let Decision::Place { host, .. } = decision {
                let has_eligible_replica = snap
                    .replicas
                    .iter()
                    .any(|v| v.host_id == host && first_exclusion(v, &input, input.now_ms).is_none());
                prop_assert!(has_eligible_replica);
            }
        }

        /// Guards against `placed_host_is_always_eligible` passing vacuously
        /// (e.g. if a bug made the placer stop ever choosing `Place`): a
        /// snapshot that always includes one guaranteed-eligible replica
        /// among the random ones must always yield `Decision::Place`.
        #[test]
        fn eligible_replica_forces_place(
            specs in proptest::collection::vec(replica_spec_strategy(), 0..=8),
        ) {
            let mut views = Vec::new();
            for (i, spec) in specs.into_iter().enumerate() {
                let host = format!("gpuX{}", i / 2);
                let replica = format!("r{}", i % 2);
                views.push(view_from_spec(&host, &replica, spec));
            }
            views.push(ready_view("gpu-forced-eligible", "r0"));

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
    fn host_replica_const_used_for_host_level_routing() {
        assert_eq!(HOST_REPLICA, "_host");
        let mut input = base_input();
        let mut hot = ready_view("gpu01", "r1");
        hot.report.load.running = Some(0);
        let snap_with_routed = {
            let mut s = snap_with(vec![hot]);
            s.routed.insert(
                ("gpu01".to_string(), HOST_REPLICA.to_string()),
                RoutedCounts {
                    req: 5,
                    tok: 100,
                    // As the reader builds it: the window starts a second
                    // before now, i.e. before the frame's reported_at_ms.
                    since_ms: NOW - 1_000,
                },
            );
            s
        };
        input.model = MODEL.into();
        let mut rng = StdRng::seed_from_u64(1);
        match placer().place(&input, &snap_with_routed, &HashMap::new(), &mut rng) {
            Decision::Place { record, .. } => {
                assert_eq!(record.pending_req, 5);
            }
            Decision::Legacy { .. } => panic!("expected Place"),
        }
    }

    #[test]
    fn other_nodes_routed_load_shifts_decision() {
        // Two identical idle hosts. Another node routed load to gpu-a in the
        // current window (this node's own ledger is empty), so gpu-a must
        // score worse and the placer must pick gpu-b — for every rng seed.
        let mut snap = snap_with(vec![ready_view("gpu-a", "r1"), ready_view("gpu-b", "r1")]);
        snap.routed.insert(
            ("gpu-a".to_string(), HOST_REPLICA.to_string()),
            RoutedCounts {
                req: 8,
                tok: 40_000,
                since_ms: NOW - 1_000,
            },
        );
        for seed in 0..32 {
            let mut rng = StdRng::seed_from_u64(seed);
            match placer().place(&base_input(), &snap, &HashMap::new(), &mut rng) {
                Decision::Place { host, .. } => assert_eq!(host, "gpu-b", "seed {seed}"),
                Decision::Legacy { .. } => panic!("expected Place"),
            }
        }
    }
}
