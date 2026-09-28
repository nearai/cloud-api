//! Test-only helper for sealing a [`ReplicaReport`] into a signed [`Envelope`],
//! using the identical algorithm `frame::open` expects: base64-STANDARD of the
//! ed25519 signature over `SIGNING_DOMAIN ++ frame_json_bytes`, with `key_id`
//! taken from the report's own `report_key_id`. Shared by this crate's tests
//! across modules (`frame`, `snapshot`, and later tasks).

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use crate::frame::{Envelope, ReplicaReport, SIGNING_DOMAIN};

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
