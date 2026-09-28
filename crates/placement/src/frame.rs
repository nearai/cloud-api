//! Signed envelope and `ReplicaReport` frame contract, matching
//! inference-proxy's `src/replica_state/report.rs` field-for-field.
//!
//! The envelope carries the report as the exact signed JSON string (`frame`),
//! so [`open`] verifies the received bytes before parsing them; it never
//! re-serializes `frame`. The envelope's `key_id` is an unsigned lookup hint
//! for picking the verifying key; the signed `report_key_id` inside `frame`
//! must agree with it and with the key that verified the signature.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::Digest;

/// Domain-separation prefix prepended to `frame` bytes before signing.
pub const SIGNING_DOMAIN: &[u8] = b"nearai-replica-report-v1\n";

#[derive(Deserialize)]
pub struct Envelope {
    pub frame: String,
    pub sig: String,
    /// Unsigned hint naming the key that signed `frame`; not trusted on its own.
    pub key_id: String,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Warming,
    Ready,
    Degraded,
    Unhealthy,
    Draining,
    Drained,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Limits {
    pub max_running: Option<u32>,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Load {
    pub running: Option<u32>,
    pub queued: Option<u32>,
    pub prefill_backlog_tokens: Option<u64>,
    pub kv_usage: Option<f64>,
    /// Non-evictable KV tokens in use, summed across ranks.
    pub kv_used_tokens: Option<u64>,
    /// Total KV cache capacity in tokens, summed across ranks.
    pub kv_capacity_tokens: Option<u64>,
    pub gen_tps: Option<f64>,
    pub cached_token_ratio: Option<f64>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct ReplicaReport {
    pub schema: u8,
    pub host_id: String,
    pub replica_id: String,
    pub boot_id: String,
    pub seq: u64,
    /// Engine's own sample time; `None` until the replica has been read once.
    pub engine_sampled_at_ms: Option<u64>,
    /// Wall-clock time the frame was sealed, after this tick's reads.
    pub reported_at_ms: u64,
    pub lifecycle_state: Lifecycle,
    pub model: String,
    pub engine: String,
    pub engine_version: Option<String>,
    pub limits: Limits,
    pub load: Load,
    pub proxy_inflight: u32,
    pub report_key_id: String,
}

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum FrameError {
    #[error("bad signature")]
    BadSig,
    #[error("bad encoding")]
    Encoding,
    #[error("parse")]
    Parse,
    #[error("key id mismatch")]
    KeyIdMismatch,
}

/// `hex(sha256(pk))[..16]`, matching inference-proxy's `ReportKey::key_id`.
pub fn key_id(pk: &VerifyingKey) -> String {
    hex::encode(sha2::Sha256::digest(pk.as_bytes()))[..16].to_string()
}

fn message(frame: &str) -> Vec<u8> {
    let mut m = SIGNING_DOMAIN.to_vec();
    m.extend_from_slice(frame.as_bytes());
    m
}

/// Verify, then parse. The signature is checked over the domain-separated
/// `frame` bytes as received; only then is `frame` parsed as JSON. Both the
/// envelope's `key_id` hint and the signed `report_key_id` inside the frame
/// must equal `key_id(pk)`, so an unverifiable or mismatched-key frame never
/// reaches routing.
pub fn open(env: &Envelope, pk: &VerifyingKey) -> Result<ReplicaReport, FrameError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&env.sig)
        .map_err(|_| FrameError::Encoding)?;
    let sig = Signature::from_bytes(
        &<[u8; 64]>::try_from(raw.as_slice()).map_err(|_| FrameError::Encoding)?,
    );
    pk.verify(&message(&env.frame), &sig)
        .map_err(|_| FrameError::BadSig)?;
    let r: ReplicaReport = serde_json::from_str(&env.frame).map_err(|_| FrameError::Parse)?;
    if r.report_key_id != env.key_id || r.report_key_id != key_id(pk) {
        return Err(FrameError::KeyIdMismatch);
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn pk() -> VerifyingKey {
        SigningKey::from_bytes(&[7u8; 32]).verifying_key()
    }

    fn fixture() -> Envelope {
        serde_json::from_str(include_str!("../tests/fixtures/envelope_ok.json")).unwrap()
    }

    #[test]
    fn golden_envelope_opens() {
        let r = open(&fixture(), &pk()).unwrap();
        assert_eq!(r.schema, 1);
        assert_eq!(r.report_key_id, key_id(&pk()));
    }

    #[test]
    fn tampered_frame_fails() {
        let mut e = fixture();
        e.frame = e.frame.replacen("\"seq\":", "\"seq\":9", 1);
        assert_eq!(open(&e, &pk()), Err(FrameError::BadSig));
    }

    #[test]
    fn wrong_key_fails() {
        let other = SigningKey::from_bytes(&[8u8; 32]).verifying_key();
        assert_eq!(open(&fixture(), &other), Err(FrameError::BadSig));
    }

    #[test]
    fn hint_mismatch_fails() {
        let mut e = fixture();
        e.key_id = "0000000000000000".into();
        assert_eq!(open(&e, &pk()), Err(FrameError::KeyIdMismatch));
    }

    #[test]
    fn malformed_sig_is_encoding() {
        let mut e = fixture();
        e.sig = "!!".into();
        assert_eq!(open(&e, &pk()), Err(FrameError::Encoding));
    }

    #[test]
    fn unknown_lifecycle_parses_as_unknown() {
        assert_eq!(
            serde_json::from_str::<Lifecycle>("\"rebooting\"").unwrap(),
            Lifecycle::Unknown
        );
    }
}
