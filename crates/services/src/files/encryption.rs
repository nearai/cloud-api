use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use thiserror::Error;

const NONCE_SIZE: usize = 12; // 96 bits for GCM

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("Invalid encryption key length: expected 32 bytes (256 bits), got {0}")]
    InvalidKeyLength(usize),
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("Invalid ciphertext: too short to contain nonce")]
    InvalidCiphertext,
}

/// Encrypts data using AES-256-GCM
/// The nonce is prepended to the ciphertext for later decryption
pub fn encrypt(data: &[u8], key: &str) -> Result<Vec<u8>, EncryptionError> {
    // Decode the key from hex string
    let key_bytes = hex::decode(key).map_err(|e| {
        EncryptionError::EncryptionFailed(format!("Invalid hex key: {}", e))
    })?;

    if key_bytes.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength(key_bytes.len()));
    }

    // Create cipher instance
    let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| {
        EncryptionError::EncryptionFailed(format!("Failed to create cipher: {}", e))
    })?;

    // Generate a random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    // Encrypt the data
    let ciphertext = cipher.encrypt(&nonce, data).map_err(|e| {
        EncryptionError::EncryptionFailed(format!("Encryption failed: {}", e))
    })?;

    // Prepend nonce to ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypts data using AES-256-GCM
/// Expects the nonce to be prepended to the ciphertext
pub fn decrypt(encrypted_data: &[u8], key: &str) -> Result<Vec<u8>, EncryptionError> {
    // Decode the key from hex string
    let key_bytes = hex::decode(key).map_err(|e| {
        EncryptionError::DecryptionFailed(format!("Invalid hex key: {}", e))
    })?;

    if key_bytes.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength(key_bytes.len()));
    }

    // Ensure we have at least a nonce
    if encrypted_data.len() < NONCE_SIZE {
        return Err(EncryptionError::InvalidCiphertext);
    }

    // Extract nonce and ciphertext
    let (nonce_bytes, ciphertext) = encrypted_data.split_at(NONCE_SIZE);
    let nonce = Nonce::from(*<&[u8; NONCE_SIZE]>::try_from(nonce_bytes).map_err(|_| {
        EncryptionError::InvalidCiphertext
    })?);

    // Create cipher instance
    let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| {
        EncryptionError::DecryptionFailed(format!("Failed to create cipher: {}", e))
    })?;

    // Decrypt the data
    let plaintext = cipher.decrypt(&nonce, ciphertext).map_err(|e| {
        EncryptionError::DecryptionFailed(format!("Decryption failed: {}", e))
    })?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        // Generate a random 256-bit key (32 bytes)
        let mut key_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut key_bytes);
        let key = hex::encode(key_bytes);

        let plaintext = b"Hello, World! This is a test message.";

        // Encrypt
        let encrypted = encrypt(plaintext, &key).expect("Encryption should succeed");

        // Verify encrypted data is longer (nonce + ciphertext + auth tag)
        assert!(encrypted.len() > plaintext.len());

        // Decrypt
        let decrypted = decrypt(&encrypted, &key).expect("Decryption should succeed");

        // Verify we got back the original data
        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_invalid_key_length() {
        let short_key = hex::encode([0u8; 16]); // 128 bits, not 256
        let data = b"test data";

        let result = encrypt(data, &short_key);
        assert!(matches!(result, Err(EncryptionError::InvalidKeyLength(16))));
    }

    #[test]
    fn test_decrypt_invalid_ciphertext() {
        let mut key_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut key_bytes);
        let key = hex::encode(key_bytes);

        // Too short to contain nonce
        let invalid_data = b"short";
        let result = decrypt(invalid_data, &key);
        assert!(matches!(result, Err(EncryptionError::InvalidCiphertext)));
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let mut key_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut key_bytes);
        let key = hex::encode(key_bytes);

        let plaintext = b"Secret message";
        let encrypted = encrypt(plaintext, &key).expect("Encryption should succeed");

        // Try to decrypt with a different key
        let mut wrong_key_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut wrong_key_bytes);
        let wrong_key = hex::encode(wrong_key_bytes);

        let result = decrypt(&encrypted, &wrong_key);
        assert!(matches!(result, Err(EncryptionError::DecryptionFailed(_))));
    }
}
