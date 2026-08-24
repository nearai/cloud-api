use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};
use uuid::Uuid;

pub const MARKER: &str = "__near_db_encrypted";

pub fn parse_key(hex_key: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_key).context("database encryption key must be hex encoded")?;
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| anyhow!("database encryption key must be 32 bytes, got {len}"))
}

pub fn encrypt(key: &[u8; 32], table: &str, column: &str, id: Uuid, plain: &str) -> Result<String> {
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
    Ok(json!({MARKER:true,"version":1,"alg":"AES-256-GCM","key_id":"s3-v1","nonce":BASE64.encode(nonce),"ciphertext":BASE64.encode(ciphertext)}).to_string())
}

pub fn decrypt(
    key: &[u8; 32],
    table: &str,
    column: &str,
    id: Uuid,
    encoded: &str,
) -> Result<String> {
    let value: Value = serde_json::from_str(encoded)?;
    ensure!(value[MARKER] == true, "missing encryption marker");
    ensure!(value["version"] == 1, "unsupported envelope version");
    ensure!(
        value["alg"] == "AES-256-GCM",
        "unsupported envelope algorithm"
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
    match serde_json::from_str::<Value>(&value) {
        Ok(envelope) if envelope[MARKER] == true => decrypt(key, table, column, id, &value),
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
    Ok(serde_json::from_str(&encrypt(
        key,
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
    if value[MARKER] != true {
        return Ok(value);
    }
    Ok(serde_json::from_str(&decrypt(
        key,
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
}
