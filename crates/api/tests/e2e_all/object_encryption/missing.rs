use super::*;
use async_trait::async_trait;
use services::files::{
    storage::{ObjectVerification, RewrapOutcome},
    FileServiceError,
};
use std::sync::atomic::{AtomicBool, Ordering};

struct DeleteAfterList {
    inner: Arc<MockStorage>,
    deleted: AtomicBool,
    fail: bool,
}

impl DeleteAfterList {
    fn new(inner: Arc<MockStorage>, fail: bool) -> Self {
        Self {
            inner,
            deleted: AtomicBool::new(false),
            fail,
        }
    }
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
    async fn object_key_id(&self, key: &str) -> Result<Option<String>, FileServiceError> {
        if self.fail {
            return Err(FileServiceError::StorageError(
                "injected storage failure".into(),
            ));
        }
        self.inner.object_key_id(key).await
    }
    async fn rewrap_object(&self, key: &str) -> Result<RewrapOutcome, FileServiceError> {
        self.inner.rewrap_object(key).await
    }
    async fn verify_object(&self, key: &str) -> Result<ObjectVerification, FileServiceError> {
        self.inner.verify_object(key).await
    }
}

async fn fixture(mode: &str, fail: bool) -> (TestServer, Arc<MockStorage>) {
    let ring = ring();
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    // Given a legacy object for execute and current objects for dry-run/verify.
    if mode == "execute" {
        storage
            .seed_raw(
                "a-deleted",
                encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                None,
                "text/plain",
            )
            .unwrap();
    } else {
        storage
            .upload("a-deleted", b"current".to_vec(), "text/plain")
            .await
            .unwrap();
    }
    storage
        .upload("b-retained", b"current".to_vec(), "text/plain")
        .await
        .unwrap();
    let wrapped: Arc<dyn StorageTrait> = Arc::new(DeleteAfterList::new(storage.clone(), fail));
    let (server, _) = server(wrapped, ring).await;
    (server, storage)
}

#[tokio::test]
async fn deleted_after_listing_is_skipped_in_all_job_modes() {
    for mode in ["dry_run", "execute", "verify"] {
        // When the listed object disappears before processing.
        let (server, storage) = fixture(mode, false).await;
        let actions = if mode == "verify" {
            ["verify"]
        } else {
            ["rewrap"]
        };
        let response = post(
            &server,
            "jobs",
            json!({"mode": mode, "actions": actions, "batch_size": 1}),
        )
        .await;
        assert_eq!(response.status_code(), 202);
        let result = wait(
            &server,
            response.json::<Value>()["job_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(result["status"], "completed");
        assert_eq!(result["progress"]["missing"], 1);
        assert_eq!(result["progress"]["missing_keys"], json!(["a-deleted"]));
        assert_eq!(result["cursor"]["complete"], true);
        assert_eq!(result["progress"]["processed"], 2);
        if mode == "verify" {
            assert_eq!(result["progress"]["pass"], true);
        } else if mode == "execute" {
            assert_eq!(result["progress"]["rewrapped"], 1);
        } else {
            assert_eq!(result["progress"]["by_key_id"], json!({"s3-v2": 1}));
        }
        assert_eq!(
            storage.object_key_id("b-retained").await.unwrap(),
            Some("s3-v2".into())
        );
    }
}

#[tokio::test]
async fn non_not_found_storage_error_fails_the_job() {
    // Given a storage adapter returning a non-NotFound failure.
    let (server, _) = fixture("dry_run", true).await;
    // When the job is submitted.
    let response = post(
        &server,
        "jobs",
        json!({"mode":"dry_run","actions":["rewrap"],"batch_size":1}),
    )
    .await;
    assert_eq!(response.status_code(), 202);
    // Then the worker reports its failure class.
    let result = wait_failed(
        &server,
        response.json::<Value>()["job_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["last_error_class"], "object_worker_failed");
}

async fn wait_failed(server: &TestServer, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = server
                .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(response.status_code(), 200);
            let body = response.json::<Value>();
            match body["status"].as_str() {
                Some("failed") => return body,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("object job failure timeout")
}
