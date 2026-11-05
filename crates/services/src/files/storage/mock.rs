use super::{page_size, FileServiceError, ObjectVerification, RewrapOutcome, StorageTrait};
use crate::files::encryption::{self, EncryptionError, ObjectKeyRing};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

#[derive(Clone)]
struct StoredObject {
    encrypted_data: Vec<u8>,
    key_id: Option<String>,
    content_type: String,
}

#[derive(Clone)]
pub struct MockStorage {
    files: Arc<RwLock<BTreeMap<String, StoredObject>>>,
    key_ring: Arc<ObjectKeyRing>,
}

impl MockStorage {
    pub fn new(encryption_key: String) -> Result<Self, EncryptionError> {
        Ok(Self::new_with_ring(Arc::new(ObjectKeyRing::from_hex(
            &encryption_key,
        )?)))
    }

    pub fn new_with_ring(key_ring: Arc<ObjectKeyRing>) -> Self {
        Self {
            files: Arc::new(RwLock::new(BTreeMap::new())),
            key_ring,
        }
    }

    pub fn with_key_ring(&self, key_ring: Arc<ObjectKeyRing>) -> Self {
        Self {
            files: Arc::clone(&self.files),
            key_ring,
        }
    }

    /// Seeds an encrypted object in the in-memory adapter for legacy-format fixtures.
    pub fn seed_raw(
        &self,
        key: &str,
        encrypted_data: Vec<u8>,
        key_id: Option<String>,
        content_type: &str,
    ) -> Result<(), FileServiceError> {
        self.files.write().map_err(lock_error)?.insert(
            key.to_owned(),
            StoredObject {
                encrypted_data,
                key_id,
                content_type: content_type.to_owned(),
            },
        );
        Ok(())
    }
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> FileServiceError {
    FileServiceError::StorageError(format!(
        "In-memory object storage lock unavailable: {error}"
    ))
}

#[async_trait]
impl StorageTrait for MockStorage {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError> {
        let encrypted_data = encryption::encrypt_with_key(&data, self.key_ring.write_key())?;
        self.seed_raw(
            key,
            encrypted_data,
            Some(self.key_ring.write_id().to_owned()),
            content_type,
        )
    }

    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
        let files = self.files.read().map_err(lock_error)?;
        let object = files.get(key).ok_or(FileServiceError::NotFound)?;
        Ok(encryption::decrypt_with_ring(
            &object.encrypted_data,
            &self.key_ring,
            object.key_id.as_deref(),
        )?
        .0)
    }

    async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
        self.files.write().map_err(lock_error)?.remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        Ok(self.files.read().map_err(lock_error)?.contains_key(key))
    }

    async fn list_keys(
        &self,
        continuation: Option<String>,
        max: i32,
    ) -> Result<(Vec<String>, Option<String>), FileServiceError> {
        let size = page_size(max)?;
        let files = self.files.read().map_err(lock_error)?;
        let mut remaining = files
            .keys()
            .filter(|key| continuation.as_ref().is_none_or(|last| *key > last));
        let keys: Vec<_> = remaining.by_ref().take(size).cloned().collect();
        let next = remaining.next().and_then(|_| keys.last().cloned());
        Ok((keys, next))
    }

    async fn object_key_id(&self, key: &str) -> Result<Option<String>, FileServiceError> {
        let files = self.files.read().map_err(lock_error)?;
        Ok(files
            .get(key)
            .ok_or(FileServiceError::NotFound)?
            .key_id
            .clone())
    }

    async fn rewrap_object(&self, key: &str) -> Result<RewrapOutcome, FileServiceError> {
        let mut files = self.files.write().map_err(lock_error)?;
        let object = files.get_mut(key).ok_or(FileServiceError::NotFound)?;
        if object.key_id.as_deref() == Some(self.key_ring.write_id()) {
            return Ok(RewrapOutcome::AlreadyCurrent);
        }
        let (plaintext, from) = match encryption::decrypt_with_ring(
            &object.encrypted_data,
            &self.key_ring,
            object.key_id.as_deref(),
        ) {
            Ok(value) => value,
            Err(_) => return Ok(RewrapOutcome::Undecryptable),
        };
        *object = StoredObject {
            encrypted_data: encryption::encrypt_with_key(&plaintext, self.key_ring.write_key())?,
            key_id: Some(self.key_ring.write_id().to_owned()),
            content_type: object.content_type.clone(),
        };
        Ok(RewrapOutcome::Rewrapped { from })
    }

    async fn verify_object(&self, key: &str) -> Result<ObjectVerification, FileServiceError> {
        let files = self.files.read().map_err(lock_error)?;
        let object = files.get(key).ok_or(FileServiceError::NotFound)?;
        Ok(ObjectVerification {
            is_current: object.key_id.as_deref() == Some(self.key_ring.write_id()),
            is_decryptable: encryption::decrypt_with_key(
                &object.encrypted_data,
                self.key_ring.write_key(),
            )
            .is_ok(),
        })
    }
}
