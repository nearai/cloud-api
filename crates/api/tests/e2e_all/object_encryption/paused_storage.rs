use super::*;
use async_trait::async_trait;
use services::files::{
    storage::{ObjectMetadata, ObjectVerification, RewrapOutcome},
    FileServiceError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

pub(super) struct PausedStorage {
    pub inner: Arc<MockStorage>,
    pub entered: Notify,
    pub release: Notify,
    calls: AtomicUsize,
    pause_on: usize,
}

impl PausedStorage {
    pub fn new(ring: Arc<ObjectKeyRing>, count: usize, pause_on: usize) -> Arc<Self> {
        let inner = Arc::new(MockStorage::new_with_ring(ring));
        for index in 0..count {
            inner
                .seed_raw(
                    &format!("workspace/{index:04}"),
                    encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                    None,
                    "text/plain",
                )
                .unwrap();
        }
        Arc::new(Self {
            inner,
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            pause_on,
        })
    }

    pub async fn wait_until_paused(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("worker did not reach the selected storage operation");
    }
}

#[async_trait]
impl StorageTrait for PausedStorage {
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
        self.inner.list_keys(continuation, max).await
    }

    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        self.inner.object_metadata(key).await
    }

    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.pause_on {
            self.entered.notify_one();
            self.release.notified().await;
        }
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
