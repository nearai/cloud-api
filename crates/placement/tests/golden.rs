//! Cross-repo contract: `fixtures/host_frame_v1.json` is a byte-exact copy of
//! inference-proxy's `src/replica_state/testdata/host_frame_v1.json`, sealed
//! with the key of seed `[7u8; 32]`. If the proxy regenerates its golden, copy
//! it here unchanged.

use ed25519_dalek::SigningKey;
use placement::frame::{self, Envelope};

#[test]
fn verifies_proxy_golden_host_frame() {
    let env: Envelope = serde_json::from_str(include_str!("fixtures/host_frame_v1.json")).unwrap();
    let pk = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
    let r = frame::open(&env, &pk).expect("golden frame verifies");
    assert_eq!(r.host_id, "host-a");
    assert_eq!(r.seq, 7);
    assert_eq!(
        r.replicas.iter().map(|x| x.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(r.replicas[0].limits.max_context_tokens, Some(131_072));
    assert_eq!(r.report_key_id, frame::key_id(&pk));
}

#[test]
fn golden_frame_rejects_one_flipped_byte() {
    let mut env: Envelope =
        serde_json::from_str(include_str!("fixtures/host_frame_v1.json")).unwrap();
    let original = env.frame.clone();
    env.frame = env.frame.replacen("\"seq\":7", "\"seq\":8", 1);
    assert_ne!(
        env.frame, original,
        "golden no longer contains \"seq\":7; update the tamper pattern"
    );
    let pk = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
    assert!(frame::open(&env, &pk).is_err());
}
