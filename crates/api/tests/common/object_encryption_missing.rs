use async_trait::async_trait;
use services::files::{
    encryption::{encrypt_with_key, ObjectKeyRing},
    storage::{MockStorage, ObjectMetadata, ObjectVerification, RewrapOutcome, StorageTrait},
    FileServiceError,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct DeleteAfterList {
    inner: Arc<MockStorage>,
    deleted: AtomicBool,
    fail: bool,
}

#[async_trait]
impl StorageTrait for DeleteAfterList {
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
        let page = self.inner.list_keys(continuation, max).await?;
        if self
            .deleted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.inner.delete("a-deleted").await?;
        }
        Ok(page)
    }
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        if self.fail {
            return Err(FileServiceError::StorageError(
                "injected storage failure".into(),
            ));
        }
        self.inner.object_metadata(key).await
    }
    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError> {
        self.inner.rewrap_object(key, max_bytes).await
    }
    async fn verify_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<ObjectVerification, FileServiceError> {
        self.inner.verify_object(key, max_bytes).await
    }
}

pub async fn storage(
    mode: &str,
    fail: bool,
    ring: Arc<ObjectKeyRing>,
) -> (Arc<dyn StorageTrait>, Arc<MockStorage>) {
    let storage = Arc::new(MockStorage::new_with_ring(ring));
    // Given legacy objects for execute and current objects for dry-run/verify.
    for key in ["a-deleted", "b-retained"] {
        if mode == "execute" {
            storage
                .seed_raw(
                    key,
                    encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                    None,
                    "text/plain",
                )
                .unwrap();
        } else {
            storage
                .upload(key, b"current".to_vec(), "text/plain")
                .await
                .unwrap();
        }
    }
    let wrapped: Arc<dyn StorageTrait> = Arc::new(DeleteAfterList {
        inner: storage.clone(),
        deleted: AtomicBool::new(false),
        fail,
    });
    (wrapped, storage)
}
