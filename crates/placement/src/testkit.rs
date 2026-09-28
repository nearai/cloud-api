//! Test-only helper for sealing a [`ReplicaReport`] into a signed [`Envelope`],
//! using the identical algorithm `frame::open` expects: base64-STANDARD of the
//! ed25519 signature over `SIGNING_DOMAIN ++ frame_json_bytes`, with `key_id`
//! taken from the report's own `report_key_id`. Shared by this crate's tests
//! across modules (`frame`, `snapshot`, and later tasks).

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use crate::decision::{AffinitySource, PlaceInput};
use crate::frame::{Envelope, Lifecycle, Limits, Load, ReplicaReport, SIGNING_DOMAIN};
use crate::snapshot::ReplicaView;

/// A fixed "now" for tests that reason about freshness windows.
pub(crate) const NOW: u64 = 1_000_000;

pub(crate) const TEST_HOST: &str = "glm53-gpu03";
pub(crate) const TEST_REPLICA: &str = "r1";
pub(crate) const TEST_MODEL: &str = "z-ai/glm-5.3-flash";

/// Seal `report` into an `Envelope` signed by `key`, matching the algorithm
/// inference-proxy uses to produce real frames.
pub(crate) fn seal(report: &ReplicaReport, key: &SigningKey) -> Envelope {
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

/// A `ReplicaView` that passes every eligibility rule as of `NOW`: `Ready`,
/// freshly sampled, idle (`running`/`queued` both `Some(0)`, so it also
/// clears `Rule::Capacity`'s fail-closed "no counts at all" check), and
/// serving `TEST_MODEL`.
pub(crate) fn view_ready() -> ReplicaView {
    ReplicaView {
        host_id: TEST_HOST.into(),
        replica_id: TEST_REPLICA.into(),
        report: ReplicaReport {
            schema: 1,
            host_id: TEST_HOST.into(),
            replica_id: TEST_REPLICA.into(),
            boot_id: "boot-a".into(),
            seq: 1,
            engine_sampled_at_ms: Some(NOW),
            reported_at_ms: NOW,
            lifecycle_state: Lifecycle::Ready,
            model: TEST_MODEL.into(),
            engine: "sglang".into(),
            engine_version: None,
            limits: Limits { max_running: None },
            load: Load {
                running: Some(0),
                queued: Some(0),
                ..Load::default()
            },
            proxy_inflight: 0,
            report_key_id: "test-key".into(),
        },
        received_ms: NOW,
    }
}

/// A `PlaceInput` matching `view_ready()`'s model, with a short prompt and no
/// long-context hosts.
pub(crate) fn input() -> PlaceInput {
    PlaceInput {
        request_id: "test-request".into(),
        model: TEST_MODEL.into(),
        prompt_tokens_est: 100,
        affinity: None,
        affinity_source: AffinitySource::None,
        long_context_hosts: Vec::new(),
        now_ms: NOW,
    }
}
