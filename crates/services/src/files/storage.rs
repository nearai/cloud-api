use super::encryption::ObjectKeyRing;
use super::FileServiceError;
use async_trait::async_trait;

mod mock;
mod s3;

pub use mock::MockStorage;
pub use s3::S3Storage;

const KEY_ID_METADATA: &str = "near-enc-key-id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewrapOutcome {
    AlreadyCurrent,
    Rewrapped { from: String },
    Undecryptable,
    RewrappedUnknownKeyId,
    UndecryptableUnknownKeyId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectVerification {
    pub is_current: bool,
    pub is_decryptable: bool,
    pub unknown_key_id: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub key_id: Option<String>,
    pub content_length: u64,
}

impl ObjectMetadata {
    fn enforce_limit(&self, max_bytes: u64, ring: &ObjectKeyRing) -> Result<(), FileServiceError> {
        if self.content_length > max_bytes {
            return Err(FileServiceError::ObjectTooLarge {
                is_current: self.key_id.as_deref() == Some(ring.write_id()),
                unknown_key_id: self
                    .key_id
                    .as_deref()
                    .is_some_and(|id| ring.lookup(id).is_none()),
            });
        }
        Ok(())
    }
}

#[async_trait]
pub trait StorageTrait: Send + Sync {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError>;
    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError>;
    async fn delete(&self, key: &str) -> Result<(), FileServiceError>;
    async fn exists(&self, key: &str) -> Result<bool, FileServiceError>;
    async fn list_keys(
        &self,
        continuation: Option<String>,
        max: i32,
    ) -> Result<(Vec<String>, Option<String>), FileServiceError>;
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError>;
    async fn object_key_id(&self, key: &str) -> Result<Option<String>, FileServiceError> {
        Ok(self.object_metadata(key).await?.key_id)
    }
    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError>;
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
