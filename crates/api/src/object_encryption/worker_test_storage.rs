use super::ObjectEncryptionState;
use async_trait::async_trait;
use database::DbPool;
use services::files::{
    encryption::ObjectKeyRing,
    storage::{MockStorage, ObjectMetadata, ObjectVerification, RewrapOutcome, StorageTrait},
    FileServiceError,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(super) enum Failure {
    Changed,
    Transient(usize),
    Persistent,
    Timeout,
    MetadataRace,
    MetadataOversizedRace,
    ListingTransient(usize),
    ListingPermanent,
}
struct FaultStorage {
    inner: MockStorage,
    failure: Failure,
    attempts: Arc<AtomicUsize>,
}
#[async_trait]
impl StorageTrait for FaultStorage {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError> {
        self.inner.upload(key, data, content_type).await
    }
    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
        self.inner.download(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
        self.inner.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        self.inner.exists(key).await
    }
    async fn list_keys(
        &self,
        continuation: Option<String>,
        max: i32,
    ) -> Result<(Vec<String>, Option<String>), FileServiceError> {
        match self.failure {
            Failure::ListingTransient(count)
                if self.attempts.fetch_add(1, Ordering::SeqCst) < count =>
            {
                Err(FileServiceError::TransientStorageError(
                    "private bucket details".into(),
                ))
            }
            Failure::ListingPermanent => {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Err(FileServiceError::StorageError(
                    "AccessDenied private bucket details".into(),
                ))
            }
            _ => self.inner.list_keys(continuation, max).await,
        }
    }
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        self.inner.object_metadata(key).await
    }
    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        match self.failure {
            Failure::ListingTransient(_) | Failure::ListingPermanent => {
                self.inner.rewrap_object(key, max_bytes).await
            }
            Failure::Changed => Err(FileServiceError::PreconditionFailed),
            Failure::Transient(count) if attempt < count => Err(
                FileServiceError::TransientStorageError("private details".into()),
            ),
            Failure::Transient(_) => self.inner.rewrap_object(key, max_bytes).await,
            Failure::Persistent => Err(FileServiceError::StorageError("private details".into())),
            Failure::Timeout => std::future::pending().await,
            Failure::MetadataOversizedRace => {
                self.inner.seed_raw(
                    key,
                    vec![0; 512],
                    Some("unconfigured".into()),
                    "text/plain",
                )?;
                self.inner.rewrap_object(key, max_bytes).await
            }
            Failure::MetadataRace => {
                self.inner.seed_raw(
                    key,
                    services::files::encryption::encrypt_with_key(b"payload", &[7; 32])?,
                    Some("unconfigured".into()),
                    "text/plain",
                )?;
                self.inner.rewrap_object(key, max_bytes).await
            }
        }
    }
    async fn verify_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<ObjectVerification, FileServiceError> {
        if matches!(self.failure, Failure::MetadataRace) {
            self.inner.seed_raw(
                key,
                services::files::encryption::encrypt_with_key(b"payload", &[9; 32])?,
                Some("unconfigured".into()),
                "text/plain",
            )?;
        }
        if matches!(self.failure, Failure::MetadataOversizedRace) {
            self.inner
                .seed_raw(key, vec![0; 512], Some("unconfigured".into()), "text/plain")?;
        }
        self.inner.verify_object(key, max_bytes).await
    }
}

pub(super) fn ring() -> Arc<ObjectKeyRing> {
    Arc::new(
        ObjectKeyRing::new(
            "current".into(),
            [9; 32],
            vec![("previous".into(), [7; 32])],
        )
        .unwrap(),
    )
}
pub(super) async fn mock() -> MockStorage {
    let storage = MockStorage::new_with_ring(ring());
    storage
        .upload("object", b"payload".to_vec(), "text/plain")
        .await
        .unwrap();
    storage
}
pub(super) async fn fault(failure: Failure) -> (ObjectEncryptionState, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let storage = FaultStorage {
        inner: mock().await,
        failure,
        attempts: attempts.clone(),
    };
    (
        ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(storage), ring())
            .with_storage_retry_interval(Duration::ZERO),
        attempts,
    )
}
