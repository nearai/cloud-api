use super::encryption::{self, ObjectKeyRing};
use super::FileServiceError;
use async_trait::async_trait;

mod mock;
mod s3;

pub use mock::MockStorage;
pub use s3::S3Storage;

const KEY_ID_METADATA: &str = "near-enc-key-id";

/// Metadata classification retained when an encrypted envelope exceeds its byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKeyState {
    /// The stored id names the configured write key; ciphertext is not authenticated.
    Current,
    /// The id names a previous key, or the legacy object has no id.
    StaleOrAbsent,
    /// The stored id is not configured in this ring.
    UnknownKeyId,
}

impl ObjectKeyState {
    fn for_key_id(key_id: Option<&str>, ring: &ObjectKeyRing) -> Self {
        match key_id {
            Some(id) if id == ring.write_id() => Self::Current,
            Some(id) if ring.lookup(id).is_none() => Self::UnknownKeyId,
            Some(_) | None => Self::StaleOrAbsent,
        }
    }
}

/// Result of a metadata-trusting execute operation, distinct from ciphertext verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewrapOutcome {
    /// Metadata already names the write key; execute did not authenticate ciphertext.
    AlreadyCurrent,
    /// Ciphertext was authenticated with `from` and rewritten with the write key.
    Rewrapped { from: String },
    /// No permitted ring key authenticated the ciphertext.
    Undecryptable,
    /// An unconfigured metadata id was recovered by trial decryption and replaced.
    RewrappedUnknownKeyId,
    /// An unconfigured metadata id was present and no ring key authenticated ciphertext.
    UndecryptableUnknownKeyId,
}

/// Independent metadata and permitted authentication results for key retirement checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectVerification {
    /// Metadata names the configured write key.
    pub is_current: bool,
    /// The write key authenticates ciphertext and is permitted by the stored key id.
    pub decrypts_under_write_key: bool,
    /// A previous key authenticates ciphertext and is permitted by the stored key id.
    pub decrypts_under_previous_key: bool,
    /// Metadata names an unconfigured id; verification never trials that ciphertext.
    pub unknown_key_id: bool,
}

impl ObjectVerification {
    fn authenticate(data: &[u8], ring: &ObjectKeyRing, key_id: Option<&str>) -> Self {
        let mut result = Self {
            is_current: key_id == Some(ring.write_id()),
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: matches!(
                ObjectKeyState::for_key_id(key_id, ring),
                ObjectKeyState::UnknownKeyId
            ),
        };
        if result.unknown_key_id {
            return result;
        }
        // Authenticate write-first, but never hide a mismatch with a known metadata id.
        if encryption::decrypt_with_key(data, ring.write_key()).is_ok() {
            result.decrypts_under_write_key = key_id.is_none() || result.is_current;
            return result;
        }
        result.decrypts_under_previous_key = ring.entries().skip(1).any(|(id, key)| {
            key_id.is_none_or(|hint| hint == id) && encryption::decrypt_with_key(data, key).is_ok()
        });
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    /// Stored key id, absent for legacy envelopes.
    pub key_id: Option<String>,
    /// Stored envelope bytes: nonce + ciphertext + authentication tag, not plaintext.
    pub content_length: u64,
}

impl ObjectMetadata {
    fn enforce_limit(&self, max_bytes: u64, ring: &ObjectKeyRing) -> Result<(), FileServiceError> {
        if self.content_length > max_bytes {
            return Err(FileServiceError::ObjectTooLarge {
                key_state: ObjectKeyState::for_key_id(self.key_id.as_deref(), ring),
            });
        }
        Ok(())
    }
}

/// Encrypted storage contract shared by the S3 adapter and in-memory test double.
/// Both adapters preserve envelope format, strict known-id reads, write-first legacy
/// trials, byte-limit classification, and metadata-versus-authentication outcomes.
#[async_trait]
pub trait StorageTrait: Send + Sync {
    /// Encrypts plaintext with the write key and stores its id and content type.
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError>;
    /// Authenticates and decrypts; a known metadata id permits only its named key.
    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError>;
    /// Removes an object; deleting an absent object succeeds.
    async fn delete(&self, key: &str) -> Result<(), FileServiceError>;
    /// Reports object absence as false, preserving storage and permission failures.
    async fn exists(&self, key: &str) -> Result<bool, FileServiceError>;
    /// Lists up to 1–1000 keys with an opaque continuation token for the next page.
    async fn list_keys(
        &self,
        continuation: Option<String>,
        max: i32,
    ) -> Result<(Vec<String>, Option<String>), FileServiceError>;
    /// Reads the key id and stored envelope length without downloading ciphertext.
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError>;
    /// Reads the stored id; absent objects fail, while legacy objects return None.
    async fn object_key_id(&self, key: &str) -> Result<Option<String>, FileServiceError> {
        Ok(self.object_metadata(key).await?.key_id)
    }
    /// Re-encrypts with the write key within `max_bytes` stored envelope bytes.
    /// Execute trusts current metadata and skips its ciphertext: run execute → verify
    /// and retire keys only after verify passes. Rewrites preserve content type and
    /// metadata. S3 uses If-Match when GET supplies an ETag, so concurrent changes or
    /// deletion fail rather than overwrite/recreate; without an ETag its PUT is
    /// unconditional. The mock holds a write lock for its atomic replacement.
    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError>;
    /// Authenticates within `max_bytes` stored envelope bytes without rewriting.
    /// Write-key authentication is checked first, then permitted previous keys.
    /// Unknown metadata ids are reported without trial decryption. Known ids never
    /// permit another key to mask a mismatch; absent ids permit the full ring.
    async fn verify_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<ObjectVerification, FileServiceError>;
}

fn page_size(max: i32) -> Result<usize, FileServiceError> {
    match usize::try_from(max) {
        Ok(size @ 1..=1000) => Ok(size),
        Ok(_) | Err(_) => Err(FileServiceError::StorageError(
            "Object page size must be between 1 and 1000".into(),
        )),
    }
}

#[cfg(test)]
mod mock_tests;
#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod s3_tests;
#[cfg(test)]
mod verification_tests;
