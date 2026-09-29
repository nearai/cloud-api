//! Signed envelope and `HostReport` frame contract, matching
//! inference-proxy's `src/replica_state/report.rs` field-for-field.
//!
//! One frame per host per tick carries every replica on that host. A
//! replica's identity is its `index` (its position in the host proxy's
//! backend order); the frame carries no model and no replica id string.
//!
//! The envelope carries the report as the exact signed JSON string (`frame`),
//! so [`open`] verifies the received bytes before parsing them; it never
//! re-serializes `frame`. The envelope's `key_id` is an unsigned lookup hint
//! for picking the verifying key; the signed `report_key_id` inside `frame`
//! must agree with it and with the key that verified the signature.

use std::collections::HashSet;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::consts::MAX_REPLICAS_PER_HOST;

/// Domain-separation prefix prepended to `frame` bytes before signing.
pub const SIGNING_DOMAIN: &[u8] = b"nearai-replica-report-v1\n";

#[derive(Deserialize)]
pub struct Envelope {
    pub frame: String,
    pub sig: String,
    /// Unsigned hint naming the key that signed `frame`; not trusted on its own.
    pub key_id: String,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
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

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
pub struct Limits {
    pub max_running: Option<u32>,
    /// Engine's max context length (prompt + output tokens).
    pub max_context_tokens: Option<u64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
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

/// One replica's state within a [`HostReport`], identified by `index`.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ReplicaState {
    pub index: u32,
    /// Engine's own sample time; `None` until the replica has been read once.
    pub engine_sampled_at_ms: Option<u64>,
    pub lifecycle_state: Lifecycle,
    pub engine_version: Option<String>,
    pub limits: Limits,
    pub load: Load,
    pub proxy_inflight: u32,
}

/// One signed frame per host per tick, carrying every replica on the host.
///
/// `engine` stays a string (the proxy writes a closed enum): this reader has
/// no use for it beyond logging, and an engine it doesn't know must not make
/// an otherwise valid frame unparseable.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct HostReport {
    pub schema: u8,
    pub host_id: String,
    pub boot_id: String,
    pub seq: u64,
    /// Wall-clock time the frame was sealed, after this tick's reads.
    pub reported_at_ms: u64,
    pub engine: String,
    pub report_key_id: String,
    pub replicas: Vec<ReplicaState>,
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
    #[error("too many replicas")]
    TooManyReplicas,
    #[error("duplicate replica index")]
    DuplicateIndex,
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
///
/// A frame with more than `MAX_REPLICAS_PER_HOST` replicas, or with two
/// replicas sharing an `index`, is rejected: the index is the slot identity,
/// so a duplicate would let two states claim one slot.
pub fn open(env: &Envelope, pk: &VerifyingKey) -> Result<HostReport, FrameError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&env.sig)
        .map_err(|_| FrameError::Encoding)?;
    let sig = Signature::from_bytes(
        &<[u8; 64]>::try_from(raw.as_slice()).map_err(|_| FrameError::Encoding)?,
    );
    // `verify_strict` rejects weak (low-order) public keys and malleable
    // signatures, which plain `verify` would accept: with a low-order key a
    // forged signature can verify arbitrary frame bytes.
    pk.verify_strict(&message(&env.frame), &sig)
        .map_err(|_| FrameError::BadSig)?;
    let r: HostReport = serde_json::from_str(&env.frame).map_err(|_| FrameError::Parse)?;
    if r.report_key_id != env.key_id || r.report_key_id != key_id(pk) {
        return Err(FrameError::KeyIdMismatch);
    }
    if r.replicas.len() > MAX_REPLICAS_PER_HOST {
        return Err(FrameError::TooManyReplicas);
    }
    let mut seen = HashSet::with_capacity(r.replicas.len());
    if !r.replicas.iter().all(|x| seen.insert(x.index)) {
        return Err(FrameError::DuplicateIndex);
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{host_report, seal, seal_json};
    use ed25519_dalek::SigningKey;

    fn sk() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn pk() -> VerifyingKey {
        sk().verifying_key()
    }

    fn fixture() -> Envelope {
        serde_json::from_str(include_str!("../tests/fixtures/host_frame_v1.json")).unwrap()
    }

    #[test]
    fn golden_envelope_opens() {
        let r = open(&fixture(), &pk()).unwrap();
        assert_eq!(r.schema, 1);
        assert_eq!(r.report_key_id, key_id(&pk()));
        assert_eq!(r.replicas.len(), 2);
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
    fn low_order_key_forgery_is_rejected() {
        // The compressed identity point decodes as a valid (but weak)
        // VerifyingKey; with R = identity and s = 0 a plain `verify` would
        // accept this signature for any message.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let weak = VerifyingKey::from_bytes(&identity).expect("identity decodes");
        assert!(weak.is_weak());
        let mut forged_sig = [0u8; 64];
        forged_sig[..32].copy_from_slice(&identity);
        let mut e = fixture();
        e.sig = base64::engine::general_purpose::STANDARD.encode(forged_sig);
        assert_eq!(open(&e, &weak), Err(FrameError::BadSig));
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

    #[test]
    fn frame_with_unknown_fields_still_opens() {
        // A newer proxy may add fields at any level; this reader ignores
        // them (no `deny_unknown_fields`) rather than dropping the host.
        let r = host_report(&pk());
        let mut v = serde_json::to_value(&r).unwrap();
        v["future_field"] = serde_json::json!({ "nested": [1, 2, 3] });
        v["replicas"][0]["future_replica_field"] = serde_json::json!("x");
        v["replicas"][0]["load"]["future_load_field"] = serde_json::json!(0.5);
        let env = seal_json(v.to_string(), &r.report_key_id, &sk());
        let opened = open(&env, &pk()).expect("unknown fields are ignored");
        assert_eq!(opened, r);
    }

    #[test]
    fn rejects_duplicate_replica_index() {
        let mut r = host_report(&pk());
        let dup = r.replicas[0].clone();
        r.replicas.push(dup);
        assert_eq!(
            open(&seal(&r, &sk()), &pk()),
            Err(FrameError::DuplicateIndex)
        );
    }

    #[test]
    fn rejects_more_than_max_replicas() {
        let mut r = host_report(&pk());
        let template = r.replicas[0].clone();
        r.replicas = (0..=MAX_REPLICAS_PER_HOST as u32)
            .map(|index| ReplicaState {
                index,
                ..template.clone()
            })
            .collect();
        assert_eq!(
            open(&seal(&r, &sk()), &pk()),
            Err(FrameError::TooManyReplicas)
        );

        // Exactly the cap is fine.
        r.replicas.pop();
        let opened = open(&seal(&r, &sk()), &pk()).unwrap();
        assert_eq!(opened.replicas.len(), MAX_REPLICAS_PER_HOST);
    }

    #[test]
    fn frame_with_no_replicas_opens() {
        let mut r = host_report(&pk());
        r.replicas.clear();
        assert!(open(&seal(&r, &sk()), &pk()).unwrap().replicas.is_empty());
    }
}
