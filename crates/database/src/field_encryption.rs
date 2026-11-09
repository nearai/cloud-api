use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const MARKER: &str = "__near_db_encrypted";
pub const DEFAULT_KEY_ID: &str = "db-v1";

pub fn validate_key_id(key_id: &str) -> Result<()> {
    ensure!(
        !key_id.is_empty()
            && key_id.len() <= 64
            && key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "database encryption key id must be 1-64 ASCII letters, digits, '.', '_' or '-'"
    );
    Ok(())
}

/// One write key and ordered previous keys used for database field reads.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct KeyRing {
    write: (String, [u8; 32]),
    read: Vec<(String, [u8; 32])>,
}

impl KeyRing {
    pub fn new(
        write_id: String,
        write_key: [u8; 32],
        previous: Vec<(String, [u8; 32])>,
    ) -> Result<Self> {
        let ring = Self {
            write: (write_id, write_key),
            read: previous,
        };
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashMap::new();
        for (id, key) in std::iter::once(&ring.write).chain(&ring.read) {
            validate_key_id(id)?;
            ensure!(ids.insert(id), "duplicate database encryption key id");
            if let Some(previous_id) = keys.insert(key, id) {
                anyhow::bail!(
                    "database encryption key ids '{}' and '{}' resolve to identical keys",
                    previous_id,
                    id
                );
            }
        }
        Ok(ring)
    }

    pub fn write_id(&self) -> &str {
        &self.write.0
    }
    pub const fn write_key(&self) -> &[u8; 32] {
        &self.write.1
    }
    pub fn read_ids(&self) -> Vec<&str> {
        std::iter::once(self.write_id())
            .chain(self.read.iter().map(|(id, _)| id.as_str()))
            .collect()
    }
    pub fn canonical_id(&self, key_id: &str) -> Option<&str> {
        std::iter::once(&self.write)
            .chain(&self.read)
            .find(|(id, _)| id == key_id)
            .map(|(id, _)| id.as_str())
    }
    pub fn lookup(&self, key_id: &str) -> Option<&[u8; 32]> {
        std::iter::once(&self.write)
            .chain(&self.read)
            .find(|(id, _)| id == key_id)
            .map(|(_, key)| key)
    }
    pub fn fingerprints(&self) -> Vec<(String, String)> {
        std::iter::once(&self.write)
            .chain(&self.read)
            .map(|(id, key)| (id.clone(), services::key_fingerprint(key)))
            .collect()
    }
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing")
            .field("read_ids", &self.read_ids())
            .finish()
    }
}

pub fn envelope_key_id(value: &Value) -> Option<&str> {
    is_envelope(value)
        .then(|| value["key_id"].as_str())
        .flatten()
}

pub fn is_envelope(value: &Value) -> bool {
    value[MARKER] == true
        && value["version"] == 1
        && value["alg"] == "AES-256-GCM"
        && value["key_id"].as_str().is_some()
        && value["nonce"].as_str().is_some()
        && value["ciphertext"].as_str().is_some()
}

pub fn parse_key(hex_key: &str) -> Result<[u8; 32]> {
    let bytes = Zeroizing::new(
        hex::decode(hex_key).context("database encryption key must be hex encoded")?,
    );
    let len = bytes.len();
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("database encryption key must be 32 bytes, got {len}"))
}

pub fn encrypt(key: &[u8; 32], table: &str, column: &str, id: Uuid, plain: &str) -> Result<String> {
    encrypt_with_key_id(key, DEFAULT_KEY_ID, table, column, id, plain)
}

pub fn encrypt_with_key_id(
    key: &[u8; 32],
    key_id: &str,
    table: &str,
    column: &str,
    id: Uuid,
    plain: &str,
) -> Result<String> {
    validate_key_id(key_id)?;
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let aad = format!("{table}:{column}:{id}");
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("invalid key"))?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plain.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("encryption failed"))?;
    Ok(json!({MARKER:true,"version":1,"alg":"AES-256-GCM","key_id":key_id,"nonce":BASE64.encode(nonce),"ciphertext":BASE64.encode(ciphertext)}).to_string())
}

#[cfg(test)]
fn decrypt(key: &[u8; 32], table: &str, column: &str, id: Uuid, encoded: &str) -> Result<String> {
    let ring = KeyRing::new(DEFAULT_KEY_ID.to_string(), *key, vec![])?;
    decrypt_with_ring(&serde_json::from_str(encoded)?, &ring, table, column, id)
}

pub fn decrypt_with_ring(
    value: &Value,
    ring: &KeyRing,
    table: &str,
    column: &str,
    id: Uuid,
) -> Result<String> {
    ensure!(value[MARKER] == true, "missing encryption marker");
    ensure!(value["version"] == 1, "unsupported envelope version");
    ensure!(
        value["alg"] == "AES-256-GCM",
        "unsupported envelope algorithm"
    );
    let key = value["key_id"]
        .as_str()
        .and_then(|id| ring.lookup(id))
        .ok_or_else(|| anyhow!("unsupported encryption key id"))?;
    let nonce: [u8; 12] = BASE64
        .decode(
            value["nonce"]
                .as_str()
                .ok_or_else(|| anyhow!("missing nonce"))?,
        )?
        .try_into()
        .map_err(|_| anyhow!("invalid nonce length"))?;
    let ciphertext = BASE64.decode(
        value["ciphertext"]
            .as_str()
            .ok_or_else(|| anyhow!("missing ciphertext"))?,
    )?;
    let aad = format!("{table}:{column}:{id}");
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("invalid key"))?;
    let plaintext = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("envelope authentication failed"))?;
    Ok(String::from_utf8(plaintext)?)
}

#[cfg(test)]
fn decrypt_if_encrypted(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    value: String,
) -> Result<String> {
    let ring = KeyRing::new(DEFAULT_KEY_ID.to_string(), *key, vec![])?;
    decrypt_if_encrypted_with_ring(&ring, table, column, id, value)
}

pub fn decrypt_if_encrypted_with_ring(
    ring: &KeyRing,
    table: &str,
    column: &str,
    id: Uuid,
    value: String,
) -> Result<String> {
    match serde_json::from_str::<Value>(&value) {
        Ok(envelope) if is_envelope(&envelope) => {
            decrypt_with_ring(&envelope, ring, table, column, id)
        }
        _ => Ok(value),
    }
}

pub fn encrypt_json(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    value: &Value,
) -> Result<Value> {
    encrypt_json_with_key_id(key, DEFAULT_KEY_ID, table, column, id, value)
}

pub fn encrypt_json_with_key_id(
    key: &[u8; 32],
    key_id: &str,
    table: &str,
    column: &str,
    id: Uuid,
    value: &Value,
) -> Result<Value> {
    Ok(serde_json::from_str(&encrypt_with_key_id(
        key,
        key_id,
        table,
        column,
        id,
        &serde_json::to_string(value)?,
    )?)?)
}

#[cfg(test)]
fn decrypt_json_if_encrypted(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    value: Value,
) -> Result<Value> {
    let ring = KeyRing::new(DEFAULT_KEY_ID.to_string(), *key, vec![])?;
    decrypt_json_if_encrypted_with_ring(&ring, table, column, id, value)
}

pub fn decrypt_json_if_encrypted_with_ring(
    ring: &KeyRing,
    table: &str,
    column: &str,
    id: Uuid,
    value: Value,
) -> Result<Value> {
    if !is_envelope(&value) {
        return Ok(value);
    }
    Ok(serde_json::from_str(&decrypt_with_ring(
        &value, ring, table, column, id,
    )?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ring_zeroizes_on_drop() {
        fn assert_zeroizes_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroizes_on_drop::<KeyRing>();
    }

    #[test]
    fn key_ring_retains_order_and_previous_envelopes() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        assert_eq!(ring.read_ids(), vec!["db-v2", "db-v1"]);
        assert_eq!(ring.write_key(), &[9; 32]);
        assert_eq!(ring.lookup("db-v1"), Some(&[7; 32]));
        assert_eq!(ring.lookup("db-v3"), None);
        let id = Uuid::new_v4();
        let encoded =
            encrypt_with_key_id(&[7; 32], "db-v1", "files", "filename", id, "old.txt").unwrap();
        let value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(envelope_key_id(&value), Some("db-v1"));
        assert_eq!(
            decrypt_with_ring(&value, &ring, "files", "filename", id).unwrap(),
            "old.txt"
        );
        assert!(decrypt_with_ring(&value, &ring, "files", "storage_key", id).is_err());
        assert!(decrypt_with_ring(&value, &ring, "responses", "filename", id).is_err());
        assert!(decrypt_with_ring(&value, &ring, "files", "filename", Uuid::new_v4()).is_err());
        let single = KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap();
        assert_eq!(
            decrypt_with_ring(&value, &single, "files", "filename", id)
                .unwrap_err()
                .to_string(),
            "unsupported encryption key id"
        );
        assert_eq!(envelope_key_id(&json!({"key_id":"db-v1"})), None);
    }

    #[test]
    fn key_ring_passthroughs_read_previous_json_and_text() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let id = Uuid::new_v4();
        let value = json!({"nested": [1, 2]});
        let encrypted =
            encrypt_json_with_key_id(&[7; 32], "db-v1", "files", "filename", id, &value).unwrap();
        assert_eq!(
            decrypt_json_if_encrypted_with_ring(&ring, "files", "filename", id, encrypted.clone())
                .unwrap(),
            value
        );
        let encoded =
            encrypt_with_key_id(&[7; 32], "db-v1", "files", "filename", id, "old.txt").unwrap();
        assert_eq!(
            decrypt_if_encrypted_with_ring(&ring, "files", "filename", id, encoded).unwrap(),
            "old.txt"
        );
        assert_eq!(
            decrypt_if_encrypted_with_ring(&ring, "files", "filename", id, "plain.txt".into())
                .unwrap(),
            "plain.txt"
        );
        let mut invalid = encrypted;
        invalid["ciphertext"] = json!("AAAA");
        assert!(decrypt_with_ring(&invalid, &ring, "files", "filename", id).is_err());
    }

    #[test]
    fn key_ring_rejects_duplicate_and_invalid_ids() {
        assert!(KeyRing::new("db-v1".into(), [7; 32], vec![("db-v1".into(), [8; 32])]).is_err());
        assert!(KeyRing::new(
            "db-v2".into(),
            [7; 32],
            vec![("db-v1".into(), [8; 32]), ("db-v1".into(), [9; 32])]
        )
        .is_err());
        assert!(KeyRing::new("bad id".into(), [7; 32], vec![]).is_err());
        assert!(KeyRing::new("db-v2".into(), [7; 32], vec![("".into(), [8; 32])]).is_err());
    }

    #[test]
    fn key_ring_rejects_identical_bytes_under_distinct_ids() {
        for previous in [
            vec![("db-v1".into(), [7; 32])],
            vec![("db-v1".into(), [8; 32]), ("db-v0".into(), [8; 32])],
        ] {
            let error = KeyRing::new("db-v2".into(), [7; 32], previous.clone())
                .expect_err("identical key bytes must not have multiple ids")
                .to_string();
            let ids = if previous.len() == 1 {
                ["db-v2", "db-v1"]
            } else {
                ["db-v1", "db-v0"]
            };
            assert_eq!(
                error,
                format!(
                    "database encryption key ids '{}' and '{}' resolve to identical keys",
                    ids[0], ids[1]
                )
            );
        }
    }

    #[test]
    fn key_ring_fingerprints_match_domain_separated_hmac_vectors() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        assert_eq!(
            ring.fingerprints(),
            vec![
                ("db-v2".into(), "825674e6c3e8ca73".into()),
                ("db-v1".into(), "820ed242bff6e2d9".into()),
            ]
        );
    }

    #[test]
    fn key_ring_fingerprints_and_debug_expose_identifiers_only() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let fingerprints = ring.fingerprints();
        assert_eq!(fingerprints[0].1, "825674e6c3e8ca73");
        assert_eq!(fingerprints[1].1, "820ed242bff6e2d9");
        assert_eq!(
            fingerprints
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["db-v2", "db-v1"]
        );
        assert_ne!(fingerprints[0].1, fingerprints[1].1);
        for (_, fingerprint) in fingerprints {
            assert_eq!(fingerprint.len(), 16);
            assert!(fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
        let debug = format!("{ring:?}");
        assert!(debug.contains("db-v2") && debug.contains("db-v1"));
        assert!(!debug.contains(&hex::encode([9; 32])));
        assert!(!debug.contains(&hex::encode([7; 32])));
        assert!(!debug.contains("[9, 9"));
    }

    #[test]
    fn parse_key_accepts_32_bytes_and_rejects_invalid_inputs() {
        assert_eq!(parse_key(&"07".repeat(32)).unwrap(), [7; 32]);
        assert!(parse_key("not-hex").is_err());
        assert!(parse_key(&"07".repeat(31)).is_err());
        assert!(parse_key(&"07".repeat(33)).is_err());
    }

    #[test]
    fn key_id_validation_rejects_empty_oversized_and_unsafe_values() {
        assert!(validate_key_id("db-2026.08_v1").is_ok());
        assert!(validate_key_id("").is_err());
        assert!(validate_key_id(&"a".repeat(65)).is_err());
        assert!(validate_key_id("db key").is_err());
    }

    #[test]
    fn envelope_round_trips_and_authenticates_context() {
        let key = [7; 32];
        let id = Uuid::new_v4();
        let encrypted = encrypt(&key, "files", "filename", id, "private.txt").unwrap();

        assert!(!encrypted.contains("private.txt"));
        assert_eq!(
            decrypt(&key, "files", "filename", id, &encrypted).unwrap(),
            "private.txt"
        );
        assert!(decrypt(&key, "files", "storage_key", id, &encrypted).is_err());
    }

    #[test]
    fn envelope_rejects_an_unexpected_key_id() {
        let encoded = encrypt(&[7; 32], "files", "filename", Uuid::nil(), "secret").unwrap();
        let mut value: Value = serde_json::from_str(&encoded).unwrap();
        value["key_id"] = json!("db-v2");
        assert!(decrypt(
            &[7; 32],
            "files",
            "filename",
            Uuid::nil(),
            &value.to_string()
        )
        .is_err());
    }

    #[test]
    fn plaintext_remains_readable_during_rollout() {
        assert_eq!(
            decrypt_if_encrypted(
                &[7; 32],
                "files",
                "filename",
                Uuid::nil(),
                "legacy.txt".into()
            )
            .unwrap(),
            "legacy.txt"
        );
    }

    #[test]
    fn user_json_with_marker_is_not_treated_as_an_envelope() {
        let id = Uuid::new_v4();
        let text = json!({MARKER: true, "user": "value"}).to_string();
        assert_eq!(
            decrypt_if_encrypted(&[7; 32], "files", "filename", id, text.clone()).unwrap(),
            text
        );

        let json = json!({MARKER: true, "user": "value"});
        assert_eq!(
            decrypt_json_if_encrypted(&[7; 32], "responses", "metadata", id, json.clone()).unwrap(),
            json
        );
    }

    #[test]
    fn json_envelope_round_trips_and_legacy_json_remains_readable() {
        let key = [9; 32];
        let id = Uuid::new_v4();
        let value = json!({"token": "secret", "nested": [1, 2, 3]});

        let encrypted = encrypt_json(&key, "mcp_connectors", "auth_config", id, &value).unwrap();
        assert_eq!(encrypted[MARKER], true);
        assert!(!encrypted.to_string().contains("secret"));
        assert_eq!(
            decrypt_json_if_encrypted(&key, "mcp_connectors", "auth_config", id, encrypted)
                .unwrap(),
            value
        );
        assert_eq!(
            decrypt_json_if_encrypted(&key, "mcp_connectors", "auth_config", id, value.clone())
                .unwrap(),
            value
        );
    }
}
