//! Chutes end-to-end-encrypted inference transport: ML-KEM-768 (FIPS 203) key
//! encapsulation + HKDF-SHA256 + ChaCha20-Poly1305 AEAD.
//!
//! This is the *only* trust-preserving way to reach a Chutes inference instance:
//! the public `llm.chutes.ai` gateway terminates TLS with a CA cert (not the
//! attested key), and instances expose only a plaintext HTTP port. The security
//! boundary is therefore this E2EE layer, keyed to the instance's attested
//! ML-KEM-768 public key (`e2e_pubkey`, bound into `report_data[0:32]` — see
//! [`super::report_data`]). Because a request is encapsulated to a *specific*
//! instance's key, only that attested instance can decrypt it, so the response
//! is cryptographically bound to the instance we verified — even through the
//! load-balancing gateway.
//!
//! **The wire protocol is matched byte-for-byte to Chutes' own clients** — the
//! `pqcrypto` Python reference (`chutes-api/scripts/test_e2e_client.py`) and,
//! decisively, Chutes' own RustCrypto reference (`github.com/chutesai/e2ee-test`,
//! `ml-kem` 0.3.2), which interoperates with their server. Sizes: ML-KEM-768
//! pubkey 1184 B, ciphertext 1088 B, shared secret 32 B.
//!
//! Request blob (raw bytes, POST to `api.chutes.ai/e2e/invoke`):
//! ```text
//!   mlkem_ct[1088] ‖ nonce[12] ‖ ciphertext ‖ tag[16]
//! ```
//! where the plaintext is `gzip(json(openai_request + {e2e_response_pk: b64}))`,
//! the AEAD key is `HKDF-SHA256(ikm=shared_secret, salt=mlkem_ct[:16],
//! info="e2e-req-v1")`, and `e2e_response_pk` is a fresh per-request ephemeral
//! ML-KEM-768 public key the instance encapsulates the reply to. The
//! non-streaming response uses the same framing with `info="e2e-resp-v1"` and is
//! gzip-compressed; streaming uses one `e2e_init` encapsulation + per-chunk
//! ChaCha20-Poly1305 frames under `info="e2e-stream-v1"` (chunks are **not**
//! gzipped). AAD is empty; each message carries its own random 12-byte nonce.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use ml_kem::kem::{Decapsulate, Encapsulate, Kem, KeyExport, TryKeyInit};
use ml_kem::{EncapsulationKey768, MlKem768};
use sha2::Sha256;
use std::io::{Read, Write};

/// ML-KEM-768 ciphertext length (bytes).
pub const MLKEM_CT_LEN: usize = 1088;
/// ML-KEM-768 public-key length (bytes).
pub const MLKEM_PK_LEN: usize = 1184;
/// ChaCha20-Poly1305 nonce length (bytes).
pub const NONCE_LEN: usize = 12;
/// Poly1305 tag length (bytes).
pub const TAG_LEN: usize = 16;
/// AEAD key / ML-KEM shared-secret length (bytes).
pub const KEY_LEN: usize = 32;

/// HKDF `info` labels — one per direction, exactly as Chutes derives them.
const INFO_REQUEST: &[u8] = b"e2e-req-v1";
const INFO_RESPONSE: &[u8] = b"e2e-resp-v1";
const INFO_STREAM: &[u8] = b"e2e-stream-v1";

/// Errors from the Chutes E2EE transport. None leak plaintext.
#[derive(Debug, thiserror::Error)]
pub enum E2eeError {
    #[error("instance e2e_pubkey is not a valid {MLKEM_PK_LEN}-byte ML-KEM-768 key")]
    InvalidPublicKey,
    #[error("E2EE blob too short: {got} bytes < minimum {min}")]
    BlobTooShort { got: usize, min: usize },
    #[error("ML-KEM decapsulation failed (ciphertext not for our ephemeral key)")]
    Decapsulation,
    #[error("AEAD open failed — wrong key, tampered ciphertext, or truncated blob")]
    AeadOpen,
    #[error("gzip {0}")]
    Gzip(String),
    #[error("request JSON serialize: {0}")]
    Serialize(String),
    #[error("base64 decode of {field}: {source}")]
    Base64 {
        field: &'static str,
        #[source]
        source: base64::DecodeError,
    },
    #[error("OS RNG unavailable: {0}")]
    Rng(String),
}

fn gzip(plain: &[u8]) -> Result<Vec<u8>, E2eeError> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(plain)
        .map_err(|e| E2eeError::Gzip(e.to_string()))?;
    enc.finish().map_err(|e| E2eeError::Gzip(e.to_string()))
}

/// Upper bound on a decompressed response/frame. Inference JSON is far smaller;
/// this just caps a malicious-but-attested instance's gzip bomb (defense in depth).
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

fn gunzip(comp: &[u8]) -> Result<Vec<u8>, E2eeError> {
    let mut dec = flate2::read::GzDecoder::new(comp).take(MAX_DECOMPRESSED + 1);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| E2eeError::Gzip(e.to_string()))?;
    if out.len() as u64 > MAX_DECOMPRESSED {
        return Err(E2eeError::Gzip(format!(
            "decompressed payload exceeds {MAX_DECOMPRESSED} bytes (possible gzip bomb)"
        )));
    }
    Ok(out)
}

/// `HKDF-SHA256(ikm=shared_secret, salt=mlkem_ct[:16], info).expand -> 32 bytes`.
/// `salt` is the first 16 bytes of the ML-KEM ciphertext (Chutes' convention).
fn derive_key(shared_secret: &[u8], mlkem_ct: &[u8], info: &[u8]) -> [u8; KEY_LEN] {
    let salt = &mlkem_ct[..16];
    let hk = Hkdf::<Sha256>::new(Some(salt), shared_secret);
    let mut key = [0u8; KEY_LEN];
    // expand only fails for absurd output lengths; 32 bytes never fails.
    hk.expand(info, &mut key)
        .expect("HKDF expand of 32 bytes is infallible");
    key
}

// `from_slice` is deprecated only as a generic-array 0.14→1.x migration nag
// (pinned transitively by chacha20poly1305 0.10); the calls are correct.
#[allow(deprecated)]
fn aead_seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    // ChaCha20Poly1305::encrypt returns ciphertext ‖ tag; AAD is empty.
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .expect("ChaCha20-Poly1305 seal is infallible for in-memory plaintext")
}

#[allow(deprecated)]
fn aead_open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    ct_and_tag: &[u8],
) -> Result<Vec<u8>, E2eeError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct_and_tag)
        .map_err(|_| E2eeError::AeadOpen)
}

fn random_nonce() -> Result<[u8; NONCE_LEN], E2eeError> {
    let mut out = [0u8; NONCE_LEN];
    // Fail closed: this runs on the request path, so a (rare) RNG failure must
    // surface as an error to the caller, never panic the task.
    getrandom::fill(&mut out).map_err(|e| E2eeError::Rng(e.to_string()))?;
    Ok(out)
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A built request blob plus the per-request ephemeral decapsulation key needed
/// to open the response/stream. Hold [`ResponseSession`] until the response (or
/// the entire stream) has been decrypted.
pub struct PreparedRequest {
    /// `mlkem_ct ‖ nonce ‖ ciphertext ‖ tag` — the raw POST body for `/e2e/invoke`.
    pub blob: Vec<u8>,
    /// Keeps the ephemeral ML-KEM secret key alive to decrypt the reply.
    pub session: ResponseSession,
}

/// Holds the per-request ephemeral ML-KEM-768 decapsulation key. The instance
/// encapsulates the response (and the stream key) to the matching public key,
/// so this must outlive the whole response.
pub struct ResponseSession {
    response_dk: ml_kem::DecapsulationKey768,
}

/// Build an E2EE request blob for `request_json` (the OpenAI request body)
/// targeting `instance_e2e_pubkey` (the base64-decoded ML-KEM-768 key from
/// `/e2e/instances` for the attested instance).
pub fn build_request(
    instance_e2e_pubkey: &[u8],
    request_json: &serde_json::Value,
) -> Result<PreparedRequest, E2eeError> {
    if instance_e2e_pubkey.len() != MLKEM_PK_LEN {
        return Err(E2eeError::InvalidPublicKey);
    }
    let instance_ek = EncapsulationKey768::new_from_slice(instance_e2e_pubkey)
        .map_err(|_| E2eeError::InvalidPublicKey)?;

    // Fresh ephemeral keypair per request; the instance encapsulates the reply
    // to `response_ek`, and `response_dk` (kept in the session) decrypts it.
    let (response_dk, response_ek) = MlKem768::generate_keypair();
    let response_pk_bytes = response_ek.to_bytes();

    // Encapsulate a shared secret to the instance's attested key.
    let (mlkem_ct, shared_secret) = instance_ek.encapsulate();
    let sym_key = derive_key(shared_secret.as_slice(), mlkem_ct.as_slice(), INFO_REQUEST);

    // Inject the client's ephemeral public key so the instance can reply.
    let mut payload = request_json.clone();
    match payload.as_object_mut() {
        Some(map) => {
            map.insert(
                "e2e_response_pk".to_string(),
                serde_json::Value::String(b64(response_pk_bytes.as_slice())),
            );
        }
        None => return Err(E2eeError::Serialize("request JSON is not an object".into())),
    }
    let json_bytes =
        serde_json::to_vec(&payload).map_err(|e| E2eeError::Serialize(e.to_string()))?;
    let compressed = gzip(&json_bytes)?;

    let nonce = random_nonce()?;
    let ct_and_tag = aead_seal(&sym_key, &nonce, &compressed);

    let mut blob = Vec::with_capacity(MLKEM_CT_LEN + NONCE_LEN + ct_and_tag.len());
    blob.extend_from_slice(mlkem_ct.as_slice());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct_and_tag);

    Ok(PreparedRequest {
        blob,
        session: ResponseSession { response_dk },
    })
}

impl ResponseSession {
    /// Decrypt a non-streaming response blob (`mlkem_ct ‖ nonce ‖ ct ‖ tag`,
    /// keyed with `info="e2e-resp-v1"`, gzip-compressed) into the OpenAI
    /// response JSON bytes.
    pub fn decrypt_response(&self, blob: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let parts = split_blob(blob)?;
        let shared_secret = self
            .response_dk
            .decapsulate_slice(parts.mlkem_ct)
            .map_err(|_| E2eeError::Decapsulation)?;
        let key = derive_key(shared_secret.as_slice(), parts.mlkem_ct, INFO_RESPONSE);
        let compressed = aead_open(&key, &parts.nonce, parts.ct_and_tag)?;
        gunzip(&compressed)
    }

    /// Derive the stream key from the SSE `e2e_init` event's ML-KEM ciphertext
    /// (base64-decoded). One encapsulation keys the whole stream.
    pub fn stream_key(&self, e2e_init_mlkem_ct: &[u8]) -> Result<StreamKey, E2eeError> {
        if e2e_init_mlkem_ct.len() != MLKEM_CT_LEN {
            return Err(E2eeError::BlobTooShort {
                got: e2e_init_mlkem_ct.len(),
                min: MLKEM_CT_LEN,
            });
        }
        let shared_secret = self
            .response_dk
            .decapsulate_slice(e2e_init_mlkem_ct)
            .map_err(|_| E2eeError::Decapsulation)?;
        Ok(StreamKey(derive_key(
            shared_secret.as_slice(),
            e2e_init_mlkem_ct,
            INFO_STREAM,
        )))
    }
}

/// The per-stream ChaCha20-Poly1305 key. Every content frame is sealed under
/// this same key with its own fresh 12-byte nonce; chunks are **not** gzipped.
pub struct StreamKey([u8; KEY_LEN]);

impl StreamKey {
    /// Decrypt one stream content frame (`nonce[12] ‖ ct ‖ tag[16]`, the
    /// base64-decoded `e2e` field of an SSE event) into the raw OpenAI SSE
    /// chunk bytes. No gzip.
    pub fn decrypt_chunk(&self, frame: &[u8]) -> Result<Vec<u8>, E2eeError> {
        if frame.len() < NONCE_LEN + TAG_LEN {
            return Err(E2eeError::BlobTooShort {
                got: frame.len(),
                min: NONCE_LEN + TAG_LEN,
            });
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&frame[..NONCE_LEN]);
        aead_open(&self.0, &nonce, &frame[NONCE_LEN..])
    }
}

/// The positional fields of a `mlkem_ct ‖ nonce ‖ ct ‖ tag` blob.
struct BlobParts<'a> {
    mlkem_ct: &'a [u8],
    nonce: [u8; NONCE_LEN],
    /// ciphertext ‖ tag — length is whatever remains after the fixed-size fields.
    ct_and_tag: &'a [u8],
}

/// Split a `mlkem_ct ‖ nonce ‖ ct ‖ tag` blob into its parts.
fn split_blob(blob: &[u8]) -> Result<BlobParts<'_>, E2eeError> {
    let min = MLKEM_CT_LEN + NONCE_LEN + TAG_LEN;
    if blob.len() < min {
        return Err(E2eeError::BlobTooShort {
            got: blob.len(),
            min,
        });
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&blob[MLKEM_CT_LEN..MLKEM_CT_LEN + NONCE_LEN]);
    Ok(BlobParts {
        mlkem_ct: &blob[..MLKEM_CT_LEN],
        nonce,
        ct_and_tag: &blob[MLKEM_CT_LEN + NONCE_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ml_kem::DecapsulationKey768;

    // ---- instance-side mirror (what a Chutes instance does), so we can prove a
    // full client→instance→client round-trip with the exact wire format. Uses
    // the same primitives, mirroring chutes/entrypoint/run.py + e2ee-test. ----

    /// Pretend to be an instance: decrypt a request blob, returning the OpenAI
    /// payload JSON and the client's ephemeral response public key (raw bytes).
    fn instance_open_request(
        instance_dk: &DecapsulationKey768,
        blob: &[u8],
    ) -> (serde_json::Value, Vec<u8>) {
        let parts = split_blob(blob).unwrap();
        let ss = instance_dk.decapsulate_slice(parts.mlkem_ct).unwrap();
        let key = derive_key(ss.as_slice(), parts.mlkem_ct, INFO_REQUEST);
        let compressed = aead_open(&key, &parts.nonce, parts.ct_and_tag).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&gunzip(&compressed).unwrap()).unwrap();
        use base64::Engine;
        let pk = base64::engine::general_purpose::STANDARD
            .decode(json["e2e_response_pk"].as_str().unwrap())
            .unwrap();
        (json, pk)
    }

    /// Pretend to be an instance: encapsulate a non-stream response to the
    /// client's ephemeral pubkey and seal the (gzipped) response JSON.
    fn instance_seal_response(
        client_response_pk: &[u8],
        response_json: &serde_json::Value,
    ) -> Vec<u8> {
        let ek = EncapsulationKey768::new_from_slice(client_response_pk).unwrap();
        let (mlkem_ct, ss) = ek.encapsulate();
        let key = derive_key(ss.as_slice(), mlkem_ct.as_slice(), INFO_RESPONSE);
        let compressed = gzip(&serde_json::to_vec(response_json).unwrap()).unwrap();
        let nonce = random_nonce().unwrap();
        let ct_and_tag = aead_seal(&key, &nonce, &compressed);
        let mut blob = Vec::new();
        blob.extend_from_slice(mlkem_ct.as_slice());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct_and_tag);
        blob
    }

    /// Instance side of streaming: emit the `e2e_init` ciphertext + a sealed
    /// content frame (no gzip), returning (init_ct, stream_frame).
    fn instance_stream(client_response_pk: &[u8], chunk: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let ek = EncapsulationKey768::new_from_slice(client_response_pk).unwrap();
        let (mlkem_ct, ss) = ek.encapsulate();
        let key = derive_key(ss.as_slice(), mlkem_ct.as_slice(), INFO_STREAM);
        let nonce = random_nonce().unwrap();
        let ct_and_tag = aead_seal(&key, &nonce, chunk);
        let mut frame = Vec::new();
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&ct_and_tag);
        (mlkem_ct.as_slice().to_vec(), frame)
    }

    fn instance_keypair() -> (DecapsulationKey768, Vec<u8>) {
        let (dk, ek) = MlKem768::generate_keypair();
        (dk, ek.to_bytes().as_slice().to_vec())
    }

    #[test]
    fn request_blob_has_expected_shape() {
        let (_dk, pk) = instance_keypair();
        let req = serde_json::json!({"model": "x", "messages": []});
        let prepared = build_request(&pk, &req).unwrap();
        // mlkem_ct(1088) + nonce(12) + ciphertext(>0) + tag(16)
        assert!(prepared.blob.len() > MLKEM_CT_LEN + NONCE_LEN + TAG_LEN);
        // the first 1088 bytes are the ML-KEM ciphertext.
        assert_eq!(&prepared.blob[..MLKEM_CT_LEN].len(), &MLKEM_CT_LEN);
    }

    #[test]
    fn full_non_stream_round_trip() {
        let (instance_dk, instance_pk) = instance_keypair();
        let req = serde_json::json!({"model": "glm", "messages": [{"role":"user","content":"hi"}]});
        let prepared = build_request(&instance_pk, &req).unwrap();

        // instance decrypts the request, recovers payload + client response pk
        let (payload, client_pk) = instance_open_request(&instance_dk, &prepared.blob);
        assert_eq!(payload["model"], "glm");
        assert!(payload.get("e2e_response_pk").is_some());

        // instance encrypts a response back; client decrypts it
        let resp = serde_json::json!({"id":"cmpl-1","choices":[{"message":{"content":"yo"}}]});
        let resp_blob = instance_seal_response(&client_pk, &resp);
        let got = prepared.session.decrypt_response(&resp_blob).unwrap();
        let got: serde_json::Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(got, resp);
    }

    #[test]
    fn full_stream_round_trip() {
        let (instance_dk, instance_pk) = instance_keypair();
        let req = serde_json::json!({"model":"glm","stream":true,"messages":[]});
        let prepared = build_request(&instance_pk, &req).unwrap();
        let (_payload, client_pk) = instance_open_request(&instance_dk, &prepared.blob);

        // instance starts the stream and emits one content chunk
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"tok\"}}]}";
        let (init_ct, frame) = instance_stream(&client_pk, chunk);

        // client derives the stream key from e2e_init, then decrypts the frame
        let sk = prepared.session.stream_key(&init_ct).unwrap();
        let got = sk.decrypt_chunk(&frame).unwrap();
        assert_eq!(got, chunk);
    }

    #[test]
    fn tampered_response_fails_closed() {
        let (instance_dk, instance_pk) = instance_keypair();
        let prepared = build_request(&instance_pk, &serde_json::json!({"model":"x"})).unwrap();
        let (_p, client_pk) = instance_open_request(&instance_dk, &prepared.blob);
        let mut resp_blob = instance_seal_response(&client_pk, &serde_json::json!({"ok":true}));
        let last = resp_blob.len() - 1;
        resp_blob[last] ^= 0xff; // flip a tag byte
        assert!(matches!(
            prepared.session.decrypt_response(&resp_blob).unwrap_err(),
            E2eeError::AeadOpen
        ));
    }

    #[test]
    fn wrong_session_cannot_decrypt() {
        // A response encapsulated to instance A's view of the client key can't be
        // opened by a different ephemeral session.
        let (instance_dk, instance_pk) = instance_keypair();
        let prepared = build_request(&instance_pk, &serde_json::json!({"model":"x"})).unwrap();
        let (_p, client_pk) = instance_open_request(&instance_dk, &prepared.blob);
        let resp_blob = instance_seal_response(&client_pk, &serde_json::json!({"ok":true}));

        let other = build_request(&instance_pk, &serde_json::json!({"model":"x"})).unwrap();
        // Decapsulation "succeeds" (ML-KEM is robust) but yields a different
        // shared secret, so the AEAD open fails.
        assert!(other.session.decrypt_response(&resp_blob).is_err());
    }

    #[test]
    fn rejects_wrong_length_pubkey() {
        // PreparedRequest isn't Debug, so match on the Result directly.
        assert!(matches!(
            build_request(&[0u8; 100], &serde_json::json!({"model":"x"})),
            Err(E2eeError::InvalidPublicKey)
        ));
    }

    #[test]
    fn rejects_short_blob() {
        let (_dk, pk) = instance_keypair();
        let prepared = build_request(&pk, &serde_json::json!({"model":"x"})).unwrap();
        assert!(matches!(
            prepared.session.decrypt_response(&[0u8; 10]).unwrap_err(),
            E2eeError::BlobTooShort { .. }
        ));
    }

    #[test]
    fn non_object_request_rejected() {
        let (_dk, pk) = instance_keypair();
        assert!(matches!(
            build_request(&pk, &serde_json::json!("not an object")),
            Err(E2eeError::Serialize(_))
        ));
    }
}
