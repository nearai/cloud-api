use super::{parse_key, EncryptionError};
use sha2::{Digest, Sha256};

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
        let mut ids = std::collections::HashSet::new();
        for id in std::iter::once(&write_id).chain(previous.iter().map(|(id, _)| id)) {
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
        }
        Ok(Self {
            write: (write_id, write_key),
            previous,
        })
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

    pub const fn write_key(&self) -> &[u8; 32] {
        &self.write.1
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entries().map(|(id, _)| id).collect()
    }

    pub fn fingerprints(&self) -> Vec<(String, String)> {
        self.entries()
            .map(|(id, key)| {
                let digest = Sha256::digest(key);
                (
                    id.to_string(),
                    hex::encode(digest.iter().take(8).copied().collect::<Vec<_>>()),
                )
            })
            .collect()
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, &[u8; 32])> {
        std::iter::once(&self.write)
            .chain(&self.previous)
            .map(|(id, key)| (id.as_str(), key))
    }
}

impl std::fmt::Debug for ObjectKeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectKeyRing")
            .field("ids", &self.ids())
            .finish()
    }
}
