use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};
use uuid::Uuid;

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

pub fn is_envelope(value: &Value) -> bool {
    value[MARKER] == true
        && value["version"] == 1
        && value["alg"] == "AES-256-GCM"
        && value["key_id"].as_str().is_some()
        && value["nonce"].as_str().is_some()
        && value["ciphertext"].as_str().is_some()
}

pub fn parse_key(hex_key: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_key).context("database encryption key must be hex encoded")?;
    let len = bytes.len();
    bytes
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

pub fn decrypt(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    encoded: &str,
) -> Result<String> {
    decrypt_with_key_id(key, DEFAULT_KEY_ID, table, column, id, encoded)
}

pub fn decrypt_with_key_id(
    key: &[u8; 32],
    expected_key_id: &str,
    table: &str,
    column: &str,
    id: Uuid,
    encoded: &str,
) -> Result<String> {
    validate_key_id(expected_key_id)?;
    let value: Value = serde_json::from_str(encoded)?;
    ensure!(value[MARKER] == true, "missing encryption marker");
    ensure!(value["version"] == 1, "unsupported envelope version");
    ensure!(
        value["alg"] == "AES-256-GCM",
        "unsupported envelope algorithm"
    );
    ensure!(
        value["key_id"] == expected_key_id,
        "unsupported encryption key id"
    );
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

pub fn decrypt_if_encrypted(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    value: String,
) -> Result<String> {
    decrypt_if_encrypted_with_key_id(key, DEFAULT_KEY_ID, table, column, id, value)
}

pub fn decrypt_if_encrypted_with_key_id(
    key: &[u8; 32],
    key_id: &str,
    table: &str,
    column: &str,
    id: Uuid,
    value: String,
) -> Result<String> {
    match serde_json::from_str::<Value>(&value) {
        Ok(envelope) if is_envelope(&envelope) => {
            decrypt_with_key_id(key, key_id, table, column, id, &value)
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

pub fn decrypt_json_if_encrypted(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    value: Value,
) -> Result<Value> {
    decrypt_json_if_encrypted_with_key_id(key, DEFAULT_KEY_ID, table, column, id, value)
}

pub fn decrypt_json_if_encrypted_with_key_id(
    key: &[u8; 32],
    key_id: &str,
    table: &str,
    column: &str,
    id: Uuid,
    value: Value,
) -> Result<Value> {
    if !is_envelope(&value) {
        return Ok(value);
    }
    Ok(serde_json::from_str(&decrypt_with_key_id(
        key,
        key_id,
        table,
        column,
        id,
        &serde_json::to_string(&value)?,
    )?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
