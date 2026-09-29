//! Verified, monotonic ingest of host frames into a routing [`Snapshot`].
//!
//! [`Ingest::accept`] is the only way a [`crate::frame::Envelope`] read off
//! Valkey becomes trusted [`ReplicaView`]s. It re-verifies the signature
//! against the attested [`KeyRegistry`] for the Redis key's host (never the
//! frame's own claimed identity), checks the frame's claimed host against
//! that same Redis key, and enforces a monotonic `seq` per host boot and a
//! non-regressing `engine_sampled_at_ms` per replica, so a stale or replayed
//! frame can never move a routing decision backwards.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;

use crate::affinity::PinTable;
use crate::consts::{MAX_FUTURE_SKEW_MS, SUPPORTED_SCHEMA};
use crate::frame::{self, Envelope, FrameError, HostReport, Lifecycle, ReplicaState};

/// One attested signing key for a host. The key event binds a key to a host
/// only; every replica in that host's frames is covered by it.
#[derive(Clone)]
pub struct HostKey {
    pub key_id: String,
    pub key: VerifyingKey,
}

/// Attested keys per host, built by the caller from attestation.
#[derive(Clone, Default)]
pub struct KeyRegistry {
    pub by_host: HashMap<String, Vec<HostKey>>,
}

/// A replica's routing identity: the host (from the Redis key) and the
/// replica's `index` within that host's frame. Identity is positional, so
/// reordering a host's backends re-aims its slots (accepted; see spec §11.9).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId {
    pub host: String,
    pub replica: u32,
}

impl SlotId {
    /// `host#replica`: the string HRW ranks by and the record logs.
    pub fn hrw_label(&self) -> String {
        format!("{}#{}", self.host, self.replica)
    }
}

/// One replica's most recently accepted state, tagged with its slot.
#[derive(Clone, Debug)]
pub struct ReplicaView {
    pub slot: SlotId,
    pub state: ReplicaState,
}

/// Routed-request counters for one slot, used to estimate pending load
/// between snapshots.
#[derive(Clone, Copy, Debug, Default)]
pub struct RoutedCounts {
    pub req: u32,
    pub tok: u64,
    pub since_ms: u64,
}

/// A point-in-time view the placer scores against.
///
/// `pins` sits behind an `Arc` because the table is large (up to the pins
/// stream cap) and mostly unchanged between reader cycles: the reader shares
/// it across snapshots and copies it only when a cycle actually changes it.
///
/// `disabled` is set by the reader while the data-plane kill switch is
/// present; every decision against such a snapshot is `Legacy(Disabled)`.
///
/// `refuse_on` is set by the reader while the data-plane refuse-on switch is
/// present. The placer does not read it: while it is unset (the default) the
/// caller runs a `Refused` decision on its legacy path instead; every other
/// decision is unchanged.
///
/// `host_boots` is each host's `boot_id` from its latest accepted frame. A
/// pin written on another boot of its host is ignored
/// ([`Snapshot::pin_boot_current`]): the host rebooted, so its cache is cold.
///
/// `routed_read_ms` is this node's clock when the reader *issued* the Valkey
/// read that produced `routed` (not when it completed). A local write
/// acknowledged before it is visible in `routed`; see
/// [`crate::score::unseen_by_read`]. `0` means unknown, which counts every
/// local ledger entry on top of `routed`.
#[derive(Default)]
pub struct Snapshot {
    pub built_ms: u64,
    pub replicas: Vec<ReplicaView>,
    pub routed: HashMap<SlotId, RoutedCounts>,
    pub routed_read_ms: u64,
    pub pins: Arc<PinTable>,
    pub disabled: bool,
    pub refuse_on: bool,
    pub host_boots: HashMap<String, String>,
}

impl Snapshot {
    /// Whether a pin to a slot on `host`, written while the host was on
    /// `boot`, still points at a warm cache: false only when both the pin's
    /// boot and the host's current boot are known and differ. A pin of
    /// unknown boot (written by an older node) is accepted, as is one whose
    /// host is not in this snapshot (it cannot be placed on anyway).
    pub fn pin_boot_current(&self, host: &str, boot: Option<&str>) -> bool {
        match (boot, self.host_boots.get(host)) {
            (Some(pinned), Some(current)) => pinned == current,
            _ => true,
        }
    }
}

/// Why a frame was not accepted into the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// The Redis key's host has no attested keys at all.
    UnknownKey,
    /// No attested key for the host verified this frame's signature.
    BadSig,
    /// The envelope's signature or key material was malformed.
    Encoding,
    /// The verified frame body was not valid JSON for `HostReport`.
    Parse,
    /// The verified frame's `report_key_id` disagreed with the key that
    /// verified it.
    KeyIdMismatch,
    /// The frame carried more than `MAX_REPLICAS_PER_HOST` replicas.
    TooManyReplicas,
    /// Two replicas in the frame shared an `index`.
    DuplicateIndex,
    /// `schema` was not the version this crate understands.
    Schema,
    /// The frame's claimed `host_id` disagreed with the Redis key it was
    /// read from.
    Mismatch,
    /// `seq` did not advance within the host's boot, the frame is from a
    /// boot the host already moved on from, or every replica's
    /// `engine_sampled_at_ms` moved backwards.
    Regressed,
    /// Some replica's `engine_sampled_at_ms` is further in the future than
    /// `MAX_FUTURE_SKEW_MS` (a skewed host clock). Rejected so it can never
    /// become the stored maximum that later, correct frames would regress from.
    Future,
}

impl Reject {
    /// snake_case name, for low-cardinality metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reject::UnknownKey => "unknown_key",
            Reject::BadSig => "bad_sig",
            Reject::Encoding => "encoding",
            Reject::Parse => "parse",
            Reject::KeyIdMismatch => "key_id_mismatch",
            Reject::TooManyReplicas => "too_many_replicas",
            Reject::DuplicateIndex => "duplicate_index",
            Reject::Schema => "schema",
            Reject::Mismatch => "mismatch",
            Reject::Regressed => "regressed",
            Reject::Future => "future",
        }
    }
}

fn map_frame_error(e: FrameError) -> Reject {
    match e {
        FrameError::BadSig => Reject::BadSig,
        FrameError::Encoding => Reject::Encoding,
        FrameError::Parse => Reject::Parse,
        FrameError::KeyIdMismatch => Reject::KeyIdMismatch,
        FrameError::TooManyReplicas => Reject::TooManyReplicas,
        FrameError::DuplicateIndex => Reject::DuplicateIndex,
    }
}

/// Per-host ingest state, keyed by the Redis key's host — never by the
/// frame's own claimed identity.
///
/// `seq` and boots are tracked per host (one frame per host per tick). Engine
/// time is tracked per replica: it is `None` until some accepted frame has
/// reported one for that replica, and it never moves backwards (a `None`
/// sample never lowers what's stored).
///
/// Boots a host has moved on from are remembered (up to `RETIRED_BOOTS`), so
/// a replayed frame from an earlier boot is rejected even when its engine
/// times do not regress (e.g. right after a reboot whose first frame had no
/// engine time yet).
///
/// A host whose accepted `boot_id` changed is queued for
/// [`Ingest::take_rebooted_hosts`], so the caller can drop pins to its now
/// cold slots.
#[derive(Default)]
pub struct Ingest {
    hosts: HashMap<String, HostState>,
    rebooted: BTreeSet<String>,
}

/// How many superseded boot ids are remembered per host.
const RETIRED_BOOTS: usize = 8;

#[derive(Clone, Default)]
struct HostState {
    boot: String,
    seq: u64,
    /// The latest accepted frame's `reported_at_ms` (the host's clock).
    reported_at_ms: u64,
    retired: Vec<String>,
    /// The frame's replicas as last accepted, by index. Only indexes in the
    /// latest accepted frame are kept, so a removed replica is forgotten.
    slots: BTreeMap<u32, SlotMemory>,
}

#[derive(Clone)]
struct SlotMemory {
    state: ReplicaState,
    /// The highest engine time accepted for this slot.
    engine_ms: Option<u64>,
}

impl Ingest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify `env` against each attested key for `redis_host` in turn, check
    /// its claimed host against the Redis key it was read from, and enforce
    /// monotonic `seq` within a host boot and non-regressing engine time per
    /// replica.
    ///
    /// `redis_host` comes from the Valkey key name, never the frame: a host
    /// whose key is compromised cannot claim to be another host by lying
    /// inside a validly signed frame.
    ///
    /// A replica whose engine time moved backwards keeps its previously
    /// accepted state while the frame's other replicas update; only a frame
    /// in which every replica regressed is rejected. A `Ready` replica whose
    /// load is null (a timed-out engine read) keeps its last good load and
    /// that load's engine time, if it had one. The returned views are
    /// exactly the frame's indexes, sorted by slot.
    ///
    /// `now_ms` is this node's clock; a frame with any replica's engine time
    /// more than `MAX_FUTURE_SKEW_MS` ahead of it is rejected as
    /// [`Reject::Future`].
    pub fn accept(
        &mut self,
        redis_host: &str,
        env: &Envelope,
        reg: &KeyRegistry,
        now_ms: u64,
    ) -> Result<Vec<ReplicaView>, Reject> {
        let keys = reg.by_host.get(redis_host).ok_or(Reject::UnknownKey)?;
        if keys.is_empty() {
            return Err(Reject::UnknownKey);
        }

        let mut verified: Option<HostReport> = None;
        let mut last_err = FrameError::BadSig;
        for hk in keys {
            match frame::open(env, &hk.key) {
                Ok(report) => {
                    verified = Some(report);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let report = verified.ok_or_else(|| map_frame_error(last_err))?;

        if report.schema != SUPPORTED_SCHEMA {
            return Err(Reject::Schema);
        }

        if report.host_id != redis_host {
            return Err(Reject::Mismatch);
        }

        let horizon = now_ms.saturating_add(MAX_FUTURE_SKEW_MS);
        if report
            .replicas
            .iter()
            .any(|r| r.engine_sampled_at_ms.is_some_and(|t| t > horizon))
        {
            return Err(Reject::Future);
        }

        let prev = self.hosts.get(redis_host);

        if let Some(host) = prev {
            // `seq` only resets on a boot change; within the same boot it
            // must strictly increase.
            if report.boot_id == host.boot && report.seq <= host.seq {
                return Err(Reject::Regressed);
            }
            // A boot this host already moved on from never comes back: a
            // frame from it is a replay.
            if host.retired.contains(&report.boot_id) {
                return Err(Reject::Regressed);
            }
        }

        // Engine time must never regress for a replica, in either boot case
        // — but only when both this frame and the remembered slot have one.
        // A replica's first frame after a (re)boot reports `None` until it's
        // been read once; `Rule::Freshness` excludes `None`-time replicas
        // from routing anyway, so it's safe to admit them here.
        let mut slots: BTreeMap<u32, SlotMemory> = BTreeMap::new();
        let mut regressed = 0usize;
        for r in &report.replicas {
            let old = prev.and_then(|h| h.slots.get(&r.index));
            let old_engine_ms = old.and_then(|o| o.engine_ms);
            if let (Some(new), Some(last), Some(old)) = (r.engine_sampled_at_ms, old_engine_ms, old)
            {
                if new < last {
                    regressed += 1;
                    slots.insert(r.index, old.clone());
                    continue;
                }
            }
            // Never lower the remembered engine time: keep the max of what
            // was known and what this frame reports, and keep the previous
            // value when this frame's is `None`.
            let engine_ms = match (r.engine_sampled_at_ms, old_engine_ms) {
                (Some(new), Some(old)) => Some(new.max(old)),
                (Some(new), None) => Some(new),
                (None, old) => old,
            };
            let mut state = r.clone();
            // A `Ready` replica with neither `running` nor `queued` is a
            // timed-out engine read (typically mid chunked prefill), not an
            // idle one. `Rule::Capacity` would drop it, so a busy replica
            // would vanish from affinity and the lane count. Carry the last
            // good load forward with the engine time it was sampled at:
            // `Rule::Freshness` then expires it if reads keep failing. Limits
            // and lifecycle still come from the new frame.
            if let Some(old) = old {
                let unread = |l: &frame::Load| l.running.is_none() && l.queued.is_none();
                if r.lifecycle_state == Lifecycle::Ready
                    && unread(&r.load)
                    && !unread(&old.state.load)
                {
                    state.load = old.state.load.clone();
                    state.engine_sampled_at_ms = old.state.engine_sampled_at_ms;
                }
            }
            slots.insert(r.index, SlotMemory { state, engine_ms });
        }
        if !report.replicas.is_empty() && regressed == report.replicas.len() {
            return Err(Reject::Regressed);
        }

        let mut retired = prev.map(|h| h.retired.clone()).unwrap_or_default();
        let boot_changed = prev.is_some_and(|host| host.boot != report.boot_id);
        if let Some(host) = prev.filter(|_| boot_changed) {
            retired.push(host.boot.clone());
            if retired.len() > RETIRED_BOOTS {
                retired.remove(0);
            }
        }
        if boot_changed {
            self.rebooted.insert(redis_host.to_string());
        }

        let views = slots
            .iter()
            .map(|(index, m)| ReplicaView {
                slot: SlotId {
                    host: redis_host.to_string(),
                    replica: *index,
                },
                state: m.state.clone(),
            })
            .collect();

        self.hosts.insert(
            redis_host.to_string(),
            HostState {
                boot: report.boot_id,
                seq: report.seq,
                reported_at_ms: report.reported_at_ms,
                retired,
                slots,
            },
        );

        Ok(views)
    }

    /// The hosts whose `boot_id` changed in an accepted frame since the last
    /// call, sorted, each reported once. A host's first frame seen by this
    /// `Ingest` is not a reboot (there is nothing to compare it with), nor is
    /// a rejected frame. The caller drops pins to these hosts' slots
    /// (`PinTable::drop_host`): a rebooted engine's prefix cache is cold.
    pub fn take_rebooted_hosts(&mut self) -> Vec<String> {
        std::mem::take(&mut self.rebooted).into_iter().collect()
    }

    /// The `reported_at_ms` of `host`'s latest accepted frame (the host's
    /// clock), if any.
    pub fn reported_at_ms(&self, host: &str) -> Option<u64> {
        self.hosts.get(host).map(|h| h.reported_at_ms)
    }

    /// The `boot_id` of `host`'s latest accepted frame, if any.
    pub fn boot_id(&self, host: &str) -> Option<&str> {
        self.hosts.get(host).map(|h| h.boot.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::FRESH_MAX_MS;
    use crate::rules::{first_exclusion, Rule};
    use crate::testkit::{replica_state, seal};
    use ed25519_dalek::SigningKey;

    const HOST: &str = "glm53-gpu03";
    /// This node's clock in tests: later than every fixture engine time.
    const T_NOW: u64 = 10_000;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn replica(index: u32, engine_ms: Option<u64>) -> ReplicaState {
        let mut r = replica_state(index);
        r.engine_sampled_at_ms = engine_ms;
        r
    }

    /// A one-replica frame (index 0, engine time 1_000).
    fn report() -> HostReport {
        HostReport {
            schema: 1,
            host_id: HOST.into(),
            boot_id: "boot-a".into(),
            seq: 1,
            reported_at_ms: 1_100,
            engine: "sglang".into(),
            report_key_id: frame::key_id(&signing_key().verifying_key()),
            replicas: vec![replica(0, Some(1_000))],
        }
    }

    fn registry() -> KeyRegistry {
        registry_with(vec![signing_key()])
    }

    fn registry_with(keys: Vec<SigningKey>) -> KeyRegistry {
        let mut by_host = HashMap::new();
        by_host.insert(
            HOST.to_string(),
            keys.iter()
                .map(|k| HostKey {
                    key_id: frame::key_id(&k.verifying_key()),
                    key: k.verifying_key(),
                })
                .collect(),
        );
        KeyRegistry { by_host }
    }

    fn accept(ingest: &mut Ingest, r: &HostReport) -> Result<Vec<ReplicaView>, Reject> {
        ingest.accept(HOST, &seal(r, &signing_key()), &registry(), T_NOW)
    }

    #[test]
    fn accepts_valid() {
        let mut ingest = Ingest::new();
        let views = accept(&mut ingest, &report()).expect("valid frame accepted");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].slot.host, HOST);
        assert_eq!(views[0].slot.replica, 0);
        assert_eq!(views[0].state.engine_sampled_at_ms, Some(1_000));
    }

    #[test]
    fn accept_returns_one_view_per_replica_index() {
        let mut ingest = Ingest::new();
        let mut r = report();
        // Out of order in the frame; returned sorted by slot.
        r.replicas = vec![
            replica(2, Some(1_000)),
            replica(0, Some(1_000)),
            replica(1, Some(1_000)),
        ];
        let views = accept(&mut ingest, &r).unwrap();
        let slots: Vec<SlotId> = views.iter().map(|v| v.slot.clone()).collect();
        assert_eq!(
            slots,
            (0..3)
                .map(|replica| SlotId {
                    host: HOST.into(),
                    replica
                })
                .collect::<Vec<_>>()
        );
        assert!(views.iter().all(|v| v.state.index == v.slot.replica));
    }

    #[test]
    fn slot_label_is_host_hash_index() {
        let slot = SlotId {
            host: "gpu03".into(),
            replica: 2,
        };
        assert_eq!(slot.hrw_label(), "gpu03#2");
    }

    #[test]
    fn tries_each_attested_key() {
        // The host's first attested key is from an older boot; the frame is
        // signed by the second one.
        let old = SigningKey::from_bytes(&[3u8; 32]);
        let reg = registry_with(vec![old, signing_key()]);
        let mut ingest = Ingest::new();
        let env = seal(&report(), &signing_key());
        assert_eq!(ingest.accept(HOST, &env, &reg, T_NOW).unwrap().len(), 1);
    }

    #[test]
    fn unknown_key_rejected() {
        let mut ingest = Ingest::new();
        let env = seal(&report(), &signing_key());
        // Registry has no entry at all for this host (e.g. a fresh boot whose
        // key hasn't reached the host map yet).
        let empty = KeyRegistry::default();
        assert_eq!(
            ingest.accept(HOST, &env, &empty, T_NOW).unwrap_err(),
            Reject::UnknownKey
        );
    }

    #[test]
    fn bad_signature_rejected() {
        let mut ingest = Ingest::new();
        let env = seal(&report(), &SigningKey::from_bytes(&[9u8; 32]));
        assert_eq!(
            ingest.accept(HOST, &env, &registry(), T_NOW).unwrap_err(),
            Reject::BadSig
        );
    }

    #[test]
    fn duplicate_index_maps_to_its_own_reject() {
        let mut ingest = Ingest::new();
        let mut r = report();
        r.replicas.push(replica(0, Some(1_000)));
        assert_eq!(accept(&mut ingest, &r).unwrap_err(), Reject::DuplicateIndex);
        assert_eq!(Reject::DuplicateIndex.as_str(), "duplicate_index");
        assert_eq!(Reject::TooManyReplicas.as_str(), "too_many_replicas");
    }

    #[test]
    fn seq_regression_rejected() {
        let mut ingest = Ingest::new();
        accept(&mut ingest, &report()).unwrap();

        let mut regressed = report();
        regressed.seq = 1; // not > last.seq
        regressed.replicas = vec![replica(0, Some(1_500))];
        assert_eq!(
            accept(&mut ingest, &regressed).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn seq_is_tracked_per_host_not_per_replica() {
        let mut ingest = Ingest::new();
        accept(&mut ingest, &report()).unwrap();

        // Same seq, but a different replica index: still a replay of the
        // host's seq, so rejected.
        let mut other = report();
        other.replicas = vec![replica(1, Some(1_500))];
        assert_eq!(accept(&mut ingest, &other).unwrap_err(), Reject::Regressed);

        let mut next = report();
        next.seq = 2;
        next.replicas = vec![replica(0, Some(1_500)), replica(1, Some(1_500))];
        assert_eq!(accept(&mut ingest, &next).unwrap().len(), 2);
    }

    #[test]
    fn engine_time_regression_rejected() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.seq = 5;
        first.replicas = vec![replica(0, Some(2_000))];
        accept(&mut ingest, &first).unwrap();

        let mut regressed = report();
        regressed.seq = 6; // seq advances...
        regressed.replicas = vec![replica(0, Some(1_500))]; // ...but engine time regresses
        assert_eq!(
            accept(&mut ingest, &regressed).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn one_regressed_replica_keeps_its_prior_state_others_update() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.replicas = vec![replica(0, Some(2_000)), replica(1, Some(2_000))];
        first.replicas[0].load.running = Some(5);
        accept(&mut ingest, &first).unwrap();

        let mut second = report();
        second.seq = 2;
        second.replicas = vec![replica(0, Some(1_500)), replica(1, Some(2_500))];
        second.replicas[0].load.running = Some(9);
        second.replicas[1].load.running = Some(7);
        let views = accept(&mut ingest, &second).expect("one replica still advanced");

        // Replica 0 regressed: its stored state (engine 2_000, running 5) stays.
        assert_eq!(views[0].slot.replica, 0);
        assert_eq!(views[0].state.engine_sampled_at_ms, Some(2_000));
        assert_eq!(views[0].state.load.running, Some(5));
        // Replica 1 advanced: it updates.
        assert_eq!(views[1].state.engine_sampled_at_ms, Some(2_500));
        assert_eq!(views[1].state.load.running, Some(7));

        // The kept state's engine time is still the floor for replica 0.
        let mut third = report();
        third.seq = 3;
        third.replicas = vec![replica(0, Some(1_900)), replica(1, Some(2_400))];
        assert_eq!(accept(&mut ingest, &third).unwrap_err(), Reject::Regressed);
    }

    #[test]
    fn all_regressed_is_rejected() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.replicas = vec![replica(0, Some(2_000)), replica(1, Some(2_000))];
        accept(&mut ingest, &first).unwrap();

        let mut second = report();
        second.seq = 2;
        second.replicas = vec![replica(0, Some(1_000)), replica(1, Some(1_999))];
        assert_eq!(accept(&mut ingest, &second).unwrap_err(), Reject::Regressed);

        // The rejected frame changed nothing: seq 2 is still usable.
        second.replicas = vec![replica(0, Some(2_100)), replica(1, Some(2_100))];
        assert_eq!(accept(&mut ingest, &second).unwrap().len(), 2);
    }

    #[test]
    fn replica_removed_between_frames_is_not_returned() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.replicas = vec![
            replica(0, Some(2_000)),
            replica(1, Some(2_000)),
            replica(2, Some(2_000)),
        ];
        accept(&mut ingest, &first).unwrap();

        let mut second = report();
        second.seq = 2;
        second.replicas = vec![replica(0, Some(2_100)), replica(2, Some(2_100))];
        let views = accept(&mut ingest, &second).unwrap();
        assert_eq!(
            views.iter().map(|v| v.slot.replica).collect::<Vec<_>>(),
            vec![0, 2]
        );

        // The removed slot's memory is gone too: when it comes back, its first
        // sample is accepted without comparing against the old one.
        let mut third = report();
        third.seq = 3;
        third.replicas = vec![replica(1, Some(1_000))];
        let views = accept(&mut ingest, &third).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].slot.replica, 1);
    }

    #[test]
    fn boot_change_is_reported_once() {
        let mut ingest = Ingest::new();

        // A host's first frame is not a reboot, nor is a later frame of the
        // same boot.
        let mut first = report();
        first.replicas = vec![replica(0, Some(1_000))];
        accept(&mut ingest, &first).unwrap();
        assert!(ingest.take_rebooted_hosts().is_empty());
        first.seq = 2;
        first.replicas = vec![replica(0, Some(2_000))];
        accept(&mut ingest, &first).unwrap();
        assert!(ingest.take_rebooted_hosts().is_empty());

        // A rejected frame from a new boot reports nothing.
        let mut rebooted = report();
        rebooted.boot_id = "boot-b".into();
        rebooted.replicas = vec![replica(0, Some(1_500))];
        assert_eq!(
            accept(&mut ingest, &rebooted).unwrap_err(),
            Reject::Regressed
        );
        assert!(ingest.take_rebooted_hosts().is_empty());

        // The accepted boot change is reported exactly once.
        rebooted.replicas = vec![replica(0, Some(3_000))];
        accept(&mut ingest, &rebooted).unwrap();
        rebooted.seq = 2;
        rebooted.replicas = vec![replica(0, Some(4_000))];
        accept(&mut ingest, &rebooted).unwrap();
        assert_eq!(ingest.take_rebooted_hosts(), vec![HOST.to_string()]);
        assert!(ingest.take_rebooted_hosts().is_empty());
    }

    #[test]
    fn new_boot_with_older_engine_time_rejected() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.replicas = vec![replica(0, Some(5_000))];
        accept(&mut ingest, &first).unwrap();

        let mut rebooted = report();
        rebooted.boot_id = "boot-b".into();
        rebooted.seq = 1; // fresh boot restarts seq
        rebooted.replicas = vec![replica(0, Some(4_000))]; // but engine time regresses
        assert_eq!(
            accept(&mut ingest, &rebooted).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn new_boot_first_frame_without_engine_time_accepted() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.replicas = vec![replica(0, Some(5_000))];
        accept(&mut ingest, &first).unwrap();

        let mut rebooted = report();
        rebooted.boot_id = "boot-b".into();
        rebooted.seq = 1; // fresh boot restarts seq
        rebooted.replicas = vec![replica(0, None)]; // not read yet since the reboot
        let views = accept(&mut ingest, &rebooted)
            .expect("a reboot's first frame with unknown engine time is accepted");
        assert_eq!(views[0].state.engine_sampled_at_ms, None);
    }

    #[test]
    fn none_engine_time_does_not_lower_last() {
        let mut ingest = Ingest::new();

        let mut first = report();
        first.seq = 1;
        first.replicas = vec![replica(0, Some(5_000))];
        accept(&mut ingest, &first).unwrap();

        let mut second = report();
        second.seq = 2;
        second.replicas = vec![replica(0, None)];
        accept(&mut ingest, &second)
            .expect("unknown engine time is accepted and doesn't lower the remembered one");

        let mut third = report();
        third.seq = 3;
        third.replicas = vec![replica(0, Some(4_000))]; // regresses vs the remembered 5_000
        assert_eq!(accept(&mut ingest, &third).unwrap_err(), Reject::Regressed);
    }

    #[test]
    fn replayed_frame_from_a_previous_boot_is_rejected() {
        // Boot A runs, the host reboots into B (first frame has no engine
        // time yet), then A's last frame is replayed with an engine time that
        // does not regress against the remembered max.
        let mut ingest = Ingest::new();

        let mut boot_a = report();
        boot_a.boot_id = "boot-a".into();
        boot_a.seq = 5;
        boot_a.replicas = vec![replica(0, Some(5_000))];
        let env_a = seal(&boot_a, &signing_key());
        ingest.accept(HOST, &env_a, &registry(), T_NOW).unwrap();

        let mut boot_b = report();
        boot_b.boot_id = "boot-b".into();
        boot_b.seq = 1;
        boot_b.replicas = vec![replica(0, None)];
        accept(&mut ingest, &boot_b).unwrap();

        assert_eq!(
            ingest.accept(HOST, &env_a, &registry(), T_NOW).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn far_future_frame_is_rejected_and_does_not_lock_out_later_frames() {
        // A host clock jumps ahead, publishes one frame, then is corrected.
        let mut ingest = Ingest::new();

        let mut skewed = report();
        skewed.seq = 1;
        skewed.replicas = vec![replica(0, Some(T_NOW + MAX_FUTURE_SKEW_MS + 60_000))];
        assert_eq!(accept(&mut ingest, &skewed).unwrap_err(), Reject::Future);

        // The skewed frame never became the stored max, so a correct frame
        // right after it is accepted rather than rejected as Regressed.
        let mut corrected = report();
        corrected.seq = 2;
        corrected.replicas = vec![replica(0, Some(T_NOW - 100))];
        accept(&mut ingest, &corrected)
            .expect("a correct frame after a rejected future frame is accepted");
    }

    #[test]
    fn future_sample_on_any_replica_rejects_the_whole_frame() {
        let mut ingest = Ingest::new();
        let mut r = report();
        r.replicas = vec![
            replica(0, Some(T_NOW)),
            replica(1, Some(T_NOW + MAX_FUTURE_SKEW_MS + 1)),
        ];
        assert_eq!(accept(&mut ingest, &r).unwrap_err(), Reject::Future);
    }

    #[test]
    fn small_future_skew_is_tolerated() {
        let mut ingest = Ingest::new();
        let mut ahead = report();
        ahead.replicas = vec![replica(0, Some(T_NOW + MAX_FUTURE_SKEW_MS))];
        accept(&mut ingest, &ahead).expect("skew within MAX_FUTURE_SKEW_MS is accepted");
    }

    /// A replica whose engine read timed out: still `Ready`, every load
    /// field null, and the previous engine time carried over (as the proxy
    /// publishes it).
    fn null_load(index: u32, engine_ms: Option<u64>) -> ReplicaState {
        let mut r = replica(index, engine_ms);
        r.load = frame::Load::default();
        r
    }

    /// gpu03 seq 10971's r1: 9 running, 3 queued, 3,104 backlog tokens.
    fn busy(index: u32, engine_ms: u64) -> ReplicaState {
        let mut r = replica(index, Some(engine_ms));
        r.load.running = Some(9);
        r.load.queued = Some(3);
        r.load.prefill_backlog_tokens = Some(3_104);
        r
    }

    fn exclusion_at(view: &ReplicaView, now_ms: u64) -> Option<Rule> {
        first_exclusion(view, &crate::testkit::input(), now_ms).map(|e| e.0)
    }

    #[test]
    fn null_load_frame_keeps_last_good_load_until_stale() {
        let mut ingest = Ingest::new();
        let mut first = report();
        first.replicas = vec![busy(0, 2_000)];
        accept(&mut ingest, &first).unwrap();

        // The next frame's read timed out. Limits (and lifecycle) still come
        // from the new frame; load and its engine time from the last good one.
        let mut second = report();
        second.seq = 2;
        second.replicas = vec![null_load(0, Some(2_000))];
        second.replicas[0].limits.max_running = Some(32);
        let views = accept(&mut ingest, &second).unwrap();
        let v = &views[0];
        assert_eq!(v.state.load, busy(0, 2_000).load);
        assert_eq!(v.state.engine_sampled_at_ms, Some(2_000));
        assert_eq!(v.state.limits.max_running, Some(32));
        assert_eq!(v.state.lifecycle_state, Lifecycle::Ready);

        // Still routable while the carried sample is fresh (not dropped by
        // the Capacity rule's null-load check), then expired by Freshness.
        assert_eq!(exclusion_at(v, 2_000 + FRESH_MAX_MS), None);
        assert_eq!(
            exclusion_at(v, 2_000 + FRESH_MAX_MS + 1),
            Some(Rule::Freshness)
        );
    }

    #[test]
    fn null_load_after_fresh_window_is_excluded_by_freshness() {
        // Reads keep failing, and a frame even claims a newer engine time:
        // the carried load keeps the time it was sampled at, so Freshness
        // expires it rather than it looking current forever.
        let mut ingest = Ingest::new();
        let mut first = report();
        first.replicas = vec![busy(0, 2_000)];
        accept(&mut ingest, &first).unwrap();

        let mut last = Vec::new();
        for (seq, engine_ms) in [(2, Some(2_000)), (3, Some(2_500)), (4, None)] {
            let mut next = report();
            next.seq = seq;
            next.replicas = vec![null_load(0, engine_ms)];
            last = accept(&mut ingest, &next).unwrap();
        }
        let v = &last[0];
        assert_eq!(v.state.load.running, Some(9));
        assert_eq!(v.state.engine_sampled_at_ms, Some(2_000));
        assert_eq!(
            exclusion_at(v, 2_000 + FRESH_MAX_MS + 1),
            Some(Rule::Freshness)
        );

        // A good read replaces the carried load as usual.
        let mut good = report();
        good.seq = 5;
        good.replicas = vec![replica(0, Some(6_000))];
        let views = accept(&mut ingest, &good).unwrap();
        assert_eq!(views[0].state.load.running, Some(0));
        assert_eq!(views[0].state.engine_sampled_at_ms, Some(6_000));
    }

    #[test]
    fn null_load_without_prior_view_stays_excluded() {
        // No last good load to carry: the null frame is kept as-is and the
        // Capacity rule still fails closed on it.
        let mut ingest = Ingest::new();
        let mut first = report();
        first.replicas = vec![null_load(0, Some(2_000))];
        let views = accept(&mut ingest, &first).unwrap();
        assert_eq!(views[0].state.load, frame::Load::default());
        assert_eq!(exclusion_at(&views[0], 2_000), Some(Rule::Capacity));

        // A second null frame has only a null view before it: still excluded.
        let mut second = report();
        second.seq = 2;
        second.replicas = vec![null_load(0, Some(2_000))];
        let views = accept(&mut ingest, &second).unwrap();
        assert_eq!(exclusion_at(&views[0], 2_000), Some(Rule::Capacity));
    }

    #[test]
    fn unsupported_schema_rejected() {
        // A correctly signed frame from an attested key, but with a schema
        // this build doesn't understand, is rejected (not guessed at).
        let mut ingest = Ingest::new();
        let mut future = report();
        future.schema = SUPPORTED_SCHEMA + 1;
        assert_eq!(accept(&mut ingest, &future).unwrap_err(), Reject::Schema);
    }

    #[test]
    fn host_with_no_keys_is_unknown_key() {
        let mut ingest = Ingest::new();
        let mut by_host = HashMap::new();
        by_host.insert(HOST.to_string(), Vec::new());
        let reg = KeyRegistry { by_host };
        let env = seal(&report(), &signing_key());
        assert_eq!(
            ingest.accept(HOST, &env, &reg, T_NOW).unwrap_err(),
            Reject::UnknownKey
        );
    }

    #[test]
    fn redis_key_mismatch_rejected() {
        // The key is attested for a second host too, but the frame (signed
        // as HOST) is read off that other host's Valkey key.
        let mut ingest = Ingest::new();
        let mut reg = registry();
        let keys = reg.by_host[HOST].clone();
        reg.by_host.insert("glm53-gpu04".to_string(), keys);
        let env = seal(&report(), &signing_key());
        assert_eq!(
            ingest.accept("glm53-gpu04", &env, &reg, T_NOW).unwrap_err(),
            Reject::Mismatch
        );
    }
}
