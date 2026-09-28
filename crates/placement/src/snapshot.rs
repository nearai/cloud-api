//! Verified, monotonic ingest of replica reports into a routing [`Snapshot`].
//!
//! [`Ingest::accept`] is the only way a [`crate::frame::Envelope`] read off
//! Valkey becomes a trusted [`ReplicaView`]. It re-verifies the signature
//! against the attested [`KeyRegistry`] for the Redis key's host (never the
//! frame's own claimed identity), checks the frame's claimed identity against
//! that same Redis key and the attested replica list, and enforces monotonic
//! `seq`/`engine_sampled_at_ms` per boot so a stale or replayed frame can
//! never move a routing decision backwards.

use std::collections::HashMap;

use ed25519_dalek::VerifyingKey;

use crate::affinity::PinTable;
use crate::consts::SUPPORTED_SCHEMA;
use crate::frame::{self, Envelope, FrameError, ReplicaReport};

/// One attested signing key for a host, naming which replicas and model it
/// is entitled to report for.
#[derive(Clone)]
pub struct HostKey {
    pub key_id: String,
    pub key: VerifyingKey,
    pub replica_ids: Vec<String>,
    pub model: String,
}

/// Attested keys per host, built by the caller from attestation (Task 7).
#[derive(Clone, Default)]
pub struct KeyRegistry {
    pub by_host: HashMap<String, Vec<HostKey>>,
}

/// A single replica's most recently accepted report, tagged with the Redis
/// key it was read from and when it was received.
#[derive(Clone, Debug)]
pub struct ReplicaView {
    pub host_id: String,
    pub replica_id: String,
    pub report: ReplicaReport,
    pub received_ms: u64,
}

/// Routed-request counters for one (host, replica), used to estimate pending
/// load between snapshots. Populated by a later task.
#[derive(Clone, Copy, Debug, Default)]
pub struct RoutedCounts {
    pub req: u32,
    pub tok: u64,
    pub since_ms: u64,
}

/// A point-in-time view the placer scores against.
#[derive(Default)]
pub struct Snapshot {
    pub built_ms: u64,
    pub replicas: Vec<ReplicaView>,
    pub routed: HashMap<(String, String), RoutedCounts>,
    pub pins: PinTable,
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
    /// The verified frame body was not valid JSON for `ReplicaReport`.
    Parse,
    /// The verified frame's `report_key_id` disagreed with the key that
    /// verified it.
    KeyIdMismatch,
    /// `schema` was not the version this crate understands.
    Schema,
    /// The frame's claimed `host_id`/`replica_id`/`model` disagreed with the
    /// Redis key it was read from, or the replica is not in the attested
    /// list for that key.
    Mismatch,
    /// `seq` or `engine_sampled_at_ms` moved backwards for this replica.
    Regressed,
}

fn map_frame_error(e: FrameError) -> Reject {
    match e {
        FrameError::BadSig => Reject::BadSig,
        FrameError::Encoding => Reject::Encoding,
        FrameError::Parse => Reject::Parse,
        FrameError::KeyIdMismatch => Reject::KeyIdMismatch,
    }
}

/// Per-replica ingest state: the last accepted boot, seq and engine time,
/// keyed by the Redis key tuple `(host_id, replica_id)` — never by the
/// frame's own claimed identity.
///
/// The remembered engine time is `None` until some accepted frame has
/// reported one (`engine_sampled_at_ms` is `None` until a replica has been
/// read once). It never moves backwards: a frame with a `None` engine time
/// is accepted without an engine-time check and never lowers what's stored.
#[derive(Default)]
pub struct Ingest {
    last: HashMap<(String, String), (String, u64, Option<u64>)>,
}

impl Ingest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify `env` against the attested keys for `redis_host`, check its
    /// claimed identity against the Redis key it was read from, and enforce
    /// monotonic `seq`/`engine_sampled_at_ms` within a boot (and non-regressing
    /// `engine_sampled_at_ms` across a boot change).
    ///
    /// `redis_host`/`redis_replica` come from the Valkey key name, never the
    /// frame: an attacker who compromises one replica's key cannot claim to be
    /// another replica by lying inside a validly-signed frame.
    pub fn accept(
        &mut self,
        redis_host: &str,
        redis_replica: &str,
        env: &Envelope,
        reg: &KeyRegistry,
        now_ms: u64,
    ) -> Result<ReplicaView, Reject> {
        let keys = reg.by_host.get(redis_host).ok_or(Reject::UnknownKey)?;
        if keys.is_empty() {
            return Err(Reject::UnknownKey);
        }

        let mut verified: Option<(ReplicaReport, &HostKey)> = None;
        let mut last_err = FrameError::BadSig;
        for hk in keys {
            match frame::open(env, &hk.key) {
                Ok(report) => {
                    verified = Some((report, hk));
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let (report, hk) = verified.ok_or_else(|| map_frame_error(last_err))?;

        if report.schema != SUPPORTED_SCHEMA {
            return Err(Reject::Schema);
        }

        if report.host_id != redis_host
            || report.replica_id != redis_replica
            || report.model != hk.model
            || !hk.replica_ids.iter().any(|r| r == redis_replica)
        {
            return Err(Reject::Mismatch);
        }

        let key = (redis_host.to_string(), redis_replica.to_string());
        let prev = self.last.get(&key).cloned();

        if let Some((last_boot, last_seq, last_engine_ms)) = &prev {
            // `seq` only resets on a boot change; within the same boot it
            // must strictly increase.
            if report.boot_id == *last_boot && report.seq <= *last_seq {
                return Err(Reject::Regressed);
            }
            // Engine time must never regress, in either boot case — but only
            // when both this frame and the remembered state have one. A
            // replica's first frame after a (re)boot reports `None` until
            // it's been read once; `Rule::Freshness` (Task 3) excludes
            // `None`-time replicas from routing anyway, so it's safe to
            // admit them here rather than reject them as regressed.
            if let (Some(engine_ms), Some(last_engine_ms)) =
                (report.engine_sampled_at_ms, last_engine_ms)
            {
                if engine_ms < *last_engine_ms {
                    return Err(Reject::Regressed);
                }
            }
        }

        // Never lower the remembered engine time: keep the max of what was
        // known and what this frame reports, and keep the previous value
        // when this frame's is `None`.
        let stored_engine_ms = match (
            report.engine_sampled_at_ms,
            prev.as_ref().and_then(|(_, _, e)| *e),
        ) {
            (Some(new), Some(old)) => Some(new.max(old)),
            (Some(new), None) => Some(new),
            (None, old) => old,
        };

        self.last
            .insert(key, (report.boot_id.clone(), report.seq, stored_engine_ms));

        Ok(ReplicaView {
            host_id: redis_host.to_string(),
            replica_id: redis_replica.to_string(),
            report,
            received_ms: now_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::seal;
    use ed25519_dalek::SigningKey;

    const HOST: &str = "glm53-gpu03";
    const REPLICA: &str = "r1";
    const MODEL: &str = "z-ai/glm-5.3-flash";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn report() -> ReplicaReport {
        let pk = signing_key().verifying_key();
        ReplicaReport {
            schema: 1,
            host_id: HOST.into(),
            replica_id: REPLICA.into(),
            boot_id: "boot-a".into(),
            seq: 1,
            engine_sampled_at_ms: Some(1_000),
            reported_at_ms: 1_100,
            lifecycle_state: crate::frame::Lifecycle::Ready,
            model: MODEL.into(),
            engine: "sglang".into(),
            engine_version: None,
            limits: Default::default(),
            load: Default::default(),
            proxy_inflight: 0,
            report_key_id: frame::key_id(&pk),
        }
    }

    fn registry() -> KeyRegistry {
        let pk = signing_key().verifying_key();
        let mut by_host = HashMap::new();
        by_host.insert(
            HOST.to_string(),
            vec![HostKey {
                key_id: frame::key_id(&pk),
                key: pk,
                replica_ids: vec![REPLICA.to_string()],
                model: MODEL.to_string(),
            }],
        );
        KeyRegistry { by_host }
    }

    #[test]
    fn accepts_valid() {
        let mut ingest = Ingest::new();
        let env = seal(&report(), &signing_key());
        let view = ingest
            .accept(HOST, REPLICA, &env, &registry(), 5_000)
            .expect("valid frame accepted");
        assert_eq!(view.host_id, HOST);
        assert_eq!(view.replica_id, REPLICA);
        assert_eq!(view.received_ms, 5_000);
        assert_eq!(view.report.seq, 1);
    }

    #[test]
    fn unknown_key_rejected() {
        let mut ingest = Ingest::new();
        let env = seal(&report(), &signing_key());
        // Registry has no entry at all for this host (e.g. a fresh boot whose
        // key hasn't reached the host map yet).
        let empty = KeyRegistry::default();
        assert_eq!(
            ingest
                .accept(HOST, REPLICA, &env, &empty, 1_000)
                .unwrap_err(),
            Reject::UnknownKey
        );
    }

    #[test]
    fn seq_regression_rejected() {
        let mut ingest = Ingest::new();
        let reg = registry();

        let first = seal(&report(), &signing_key());
        ingest.accept(HOST, REPLICA, &first, &reg, 1_000).unwrap();

        let mut regressed = report();
        regressed.seq = 1; // not > last.seq
        let env = seal(&regressed, &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 2_000).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn engine_time_regression_rejected() {
        let mut ingest = Ingest::new();
        let reg = registry();

        let mut first = report();
        first.seq = 5;
        first.engine_sampled_at_ms = Some(2_000);
        let env = seal(&first, &signing_key());
        ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap();

        let mut regressed = report();
        regressed.seq = 6; // seq advances...
        regressed.engine_sampled_at_ms = Some(1_500); // ...but engine time regresses
        let env = seal(&regressed, &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 2_000).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn new_boot_with_older_engine_time_rejected() {
        let mut ingest = Ingest::new();
        let reg = registry();

        let mut first = report();
        first.engine_sampled_at_ms = Some(5_000);
        let env = seal(&first, &signing_key());
        ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap();

        let mut rebooted = report();
        rebooted.boot_id = "boot-b".into();
        rebooted.seq = 1; // fresh boot restarts seq
        rebooted.engine_sampled_at_ms = Some(4_000); // but engine time regresses
        let env = seal(&rebooted, &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 2_000).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn new_boot_first_frame_without_engine_time_accepted() {
        let mut ingest = Ingest::new();
        let reg = registry();

        let mut first = report();
        first.engine_sampled_at_ms = Some(5_000);
        let env = seal(&first, &signing_key());
        ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap();

        let mut rebooted = report();
        rebooted.boot_id = "boot-b".into();
        rebooted.seq = 1; // fresh boot restarts seq
        rebooted.engine_sampled_at_ms = None; // not read yet since the reboot
        let env = seal(&rebooted, &signing_key());
        let view = ingest
            .accept(HOST, REPLICA, &env, &reg, 2_000)
            .expect("a reboot's first frame with unknown engine time is accepted");
        assert_eq!(view.report.boot_id, "boot-b");
    }

    #[test]
    fn none_engine_time_does_not_lower_last() {
        let mut ingest = Ingest::new();
        let reg = registry();

        let mut first = report();
        first.seq = 1;
        first.engine_sampled_at_ms = Some(5_000);
        let env = seal(&first, &signing_key());
        ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap();

        let mut second = report();
        second.seq = 2;
        second.engine_sampled_at_ms = None;
        let env = seal(&second, &signing_key());
        ingest
            .accept(HOST, REPLICA, &env, &reg, 2_000)
            .expect("unknown engine time is accepted and doesn't lower the remembered one");

        let mut third = report();
        third.seq = 3;
        third.engine_sampled_at_ms = Some(4_000); // regresses vs the remembered 5_000
        let env = seal(&third, &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 3_000).unwrap_err(),
            Reject::Regressed
        );
    }

    #[test]
    fn host_with_no_keys_is_unknown_key() {
        let mut ingest = Ingest::new();
        let mut by_host = HashMap::new();
        by_host.insert(HOST.to_string(), Vec::new());
        let reg = KeyRegistry { by_host };
        let env = seal(&report(), &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap_err(),
            Reject::UnknownKey
        );
    }

    #[test]
    fn redis_key_mismatch_rejected() {
        let mut ingest = Ingest::new();
        let pk = signing_key().verifying_key();
        // The key is attested for this host under both replica ids, but the
        // frame itself (still signed for REPLICA) is read off a Valkey key
        // for a different replica than it claims to be.
        let mut by_host = HashMap::new();
        by_host.insert(
            HOST.to_string(),
            vec![HostKey {
                key_id: frame::key_id(&pk),
                key: pk,
                replica_ids: vec![REPLICA.to_string(), "r2".to_string()],
                model: MODEL.to_string(),
            }],
        );
        let reg = KeyRegistry { by_host };
        let env = seal(&report(), &signing_key());
        assert_eq!(
            ingest.accept(HOST, "r2", &env, &reg, 1_000).unwrap_err(),
            Reject::Mismatch
        );
    }

    #[test]
    fn replica_not_in_attested_list_rejected() {
        let mut ingest = Ingest::new();
        let pk = signing_key().verifying_key();
        let mut by_host = HashMap::new();
        by_host.insert(
            HOST.to_string(),
            vec![HostKey {
                key_id: frame::key_id(&pk),
                key: pk,
                replica_ids: vec!["r2".to_string()], // r1 not attested
                model: MODEL.to_string(),
            }],
        );
        let reg = KeyRegistry { by_host };
        let env = seal(&report(), &signing_key());
        assert_eq!(
            ingest.accept(HOST, REPLICA, &env, &reg, 1_000).unwrap_err(),
            Reject::Mismatch
        );
    }
}
