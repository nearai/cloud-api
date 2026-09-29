//! Test-only helpers shared by this crate's tests across modules.
//!
//! [`seal`] signs a [`HostReport`] into an [`Envelope`] with the identical
//! algorithm `frame::open` expects: base64-STANDARD of the ed25519 signature
//! over `SIGNING_DOMAIN ++ frame_json_bytes`, with `key_id` taken from the
//! report's own `report_key_id`.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

use crate::decision::{AffinitySource, PlaceInput};
use crate::frame::{self, Envelope, HostReport, Lifecycle, Load, ReplicaState, SIGNING_DOMAIN};
use crate::snapshot::{ReplicaView, SlotId};

/// A fixed "now" for tests that reason about freshness windows.
pub(crate) const NOW: u64 = 1_000_000;

pub(crate) const TEST_HOST: &str = "glm53-gpu03";
pub(crate) const TEST_MODEL: &str = "z-ai/glm-5.3-flash";

/// Seal `report` into an `Envelope` signed by `key`, matching the algorithm
/// inference-proxy uses to produce real frames.
pub(crate) fn seal(report: &HostReport, key: &SigningKey) -> Envelope {
    let frame = serde_json::to_string(report).expect("report serializes");
    let mut message = SIGNING_DOMAIN.to_vec();
    message.extend_from_slice(frame.as_bytes());
    let sig = key.sign(&message);
    Envelope {
        frame,
        sig: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        key_id: report.report_key_id.clone(),
    }
}

/// A replica state that passes every eligibility rule as of `NOW`: `Ready`,
/// freshly sampled, idle (`running`/`queued` both `Some(0)`, so it also
/// clears `Rule::Capacity`'s fail-closed "no counts at all" check), and with
/// no declared limits.
pub(crate) fn replica_state(index: u32) -> ReplicaState {
    ReplicaState {
        index,
        engine_sampled_at_ms: Some(NOW),
        lifecycle_state: Lifecycle::Ready,
        engine_version: None,
        limits: Default::default(),
        load: Load {
            running: Some(0),
            queued: Some(0),
            ..Load::default()
        },
        proxy_inflight: 0,
    }
}

/// A one-replica host frame for `TEST_HOST`, keyed to `pk`.
pub(crate) fn host_report(pk: &VerifyingKey) -> HostReport {
    HostReport {
        schema: 1,
        host_id: TEST_HOST.into(),
        boot_id: "boot-a".into(),
        seq: 1,
        reported_at_ms: NOW,
        engine: "sglang".into(),
        report_key_id: frame::key_id(pk),
        replicas: vec![replica_state(0)],
    }
}

/// An eligible view of slot `host#replica` (see [`replica_state`]).
pub(crate) fn view(host: &str, replica: u32) -> ReplicaView {
    ReplicaView {
        slot: slot(host, replica),
        state: replica_state(replica),
    }
}

/// The eligible view of `TEST_HOST#0`.
pub(crate) fn view_ready() -> ReplicaView {
    view(TEST_HOST, 0)
}

pub(crate) fn slot(host: &str, replica: u32) -> SlotId {
    SlotId {
        host: host.into(),
        replica,
    }
}

/// A short, keyless `PlaceInput` for `TEST_MODEL` with no context
/// requirement.
pub(crate) fn input() -> PlaceInput {
    PlaceInput {
        model: TEST_MODEL.into(),
        prompt_tokens: 100,
        context_tokens: None,
        heavy: false,
        priority: 0,
        affinity: None,
        affinity_source: AffinitySource::None,
        now_ms: NOW,
    }
}
