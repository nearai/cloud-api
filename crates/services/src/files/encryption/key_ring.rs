use super::{parse_key, EncryptionError};
use ring::hmac;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// One write key and ordered previous keys for object reads.
pub struct ObjectKeyRing {
    write: (String, [u8; 32]),
    previous: Vec<(String, [u8; 32])>,
}

impl ObjectKeyRing {
    pub fn new(
        write_id: String,
        write_key: [u8; 32],
        previous: Vec<(String, [u8; 32])>,
    ) -> Result<Self, EncryptionError> {
        let ring = Self {
            write: (write_id, write_key),
            previous,
        };
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for (id, key) in ring.entries() {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(EncryptionError::InvalidKeyId);
            }
            if !ids.insert(id) {
                return Err(EncryptionError::DuplicateKeyId);
            }
            if !keys.insert(key) {
                return Err(EncryptionError::DuplicateKeyMaterial);
            }
        }
        Ok(ring)
    }

    pub fn from_hex(key: &str) -> Result<Self, EncryptionError> {
        Self::new("s3-legacy".into(), parse_key(key)?, Vec::new())
    }

    pub fn lookup(&self, id: &str) -> Option<&[u8; 32]> {
        self.entries()
            .find(|(key_id, _)| *key_id == id)
            .map(|(_, key)| key)
    }

    pub fn write_id(&self) -> &str {
        &self.write.0
    }

    /// Borrows key material for cryptographic operations; callers must not copy it out.
    /// No accessor returns an owned key.
    pub const fn write_key(&self) -> &[u8; 32] {
        &self.write.1
    }

    /// Signs opaque state with the current write key.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, self.write_key()),
            message,
        )
        .as_ref()
        .to_vec()
    }

    /// Verifies opaque state with a configured read key in constant time.
    pub fn verify(&self, id: &str, message: &[u8], tag: &[u8]) -> bool {
        self.lookup(id).is_some_and(|key| {
            hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), message, tag).is_ok()
        })
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entries().map(|(id, _)| id).collect()
    }

    pub fn fingerprints(&self) -> Vec<(String, String)> {
        self.entries()
            .map(|(id, key)| (id.to_string(), crate::key_fingerprint(key)))
            .collect()
    }

    pub(in crate::files) fn entries(&self) -> impl Iterator<Item = (&str, &[u8; 32])> {
        std::iter::once(&self.write)
            .chain(&self.previous)
            .map(|(id, key)| (id.as_str(), key))
    }
}

impl Zeroize for ObjectKeyRing {
    fn zeroize(&mut self) {
        self.write.zeroize();
        self.previous.zeroize();
    }
}

impl ZeroizeOnDrop for ObjectKeyRing {}

impl Drop for ObjectKeyRing {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for ObjectKeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectKeyRing")
            .field("ids", &self.ids())
            .finish()
    }
}
