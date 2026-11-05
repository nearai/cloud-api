use super::*;
use async_trait::async_trait;
use axum_test::TestServer;
use serde_json::{json, Value};
use services::files::{
    storage::{MockStorage, ObjectMetadata, ObjectVerification, RewrapOutcome},
    FileServiceError,
};

struct DeletedAfterListing {
    inner: MockStorage,
    fail: bool,
}

#[async_trait]
impl StorageTrait for DeletedAfterListing {
    async fn upload(&self, key: &str, data: Vec<u8>, kind: &str) -> Result<(), FileServiceError> {
        self.inner.upload(key, data, kind).await
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
        self.inner.delete("a-deleted").await?;
        Ok(page)
    }
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        if self.fail {
            return Err(FileServiceError::StorageError("access denied".into()));
        }
        self.inner.object_metadata(key).await
    }
    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError> {
        if self.fail {
            return Err(FileServiceError::StorageError("access denied".into()));
        }
        self.inner.rewrap_object(key, max_bytes).await
    }
    async fn verify_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<ObjectVerification, FileServiceError> {
        if self.fail {
            return Err(FileServiceError::StorageError("access denied".into()));
        }
        self.inner.verify_object(key, max_bytes).await
    }
}

async fn fixture(fail: bool) -> ObjectEncryptionState {
    let ring = Arc::new(ObjectKeyRing::new("current".into(), [9; 32], vec![]).unwrap());
    let inner = MockStorage::new_with_ring(ring.clone());
    for key in ["a-deleted", "b-retained"] {
        inner
            .upload(key, b"content".to_vec(), "text/plain")
            .await
            .unwrap();
    }
    ObjectEncryptionState::new(
        DbPool::uninitialized(),
        Arc::new(DeletedAfterListing { inner, fail }),
        ring,
    )
}

#[tokio::test]
async fn scan_skips_objects_deleted_after_listing() {
    // Given an adapter that deletes a key only after returning its listing.
    let state = fixture(false).await;
    let app = axum::Router::new().route(
        "/scan",
        axum::routing::post(move |body| {
            let state = state.clone();
            async move {
                endpoints::scan(
                    axum::Extension(state),
                    axum::Extension(super::http_tests::admin()),
                    body,
                )
                .await
            }
        }),
    );
    let server = TestServer::builder().mock_transport().build(app);
    // When scanning through the HTTP handler.
    let response = server.post("/scan").json(&json!({})).await;
    // Then the surviving object is inventoried and the missing key advances the cursor.
    response.assert_status_ok();
    let body = response.json::<Value>();
    assert_eq!(body["totals"]["missing"], 1);
    assert_eq!(body["totals"]["missing_keys"], json!(["a-deleted"]));
    assert_eq!(body["totals"]["by_key_id"], json!({"current":1}));
    assert_eq!(body["totals"]["processed"], 2);
    assert_eq!(body["cursor"]["last_key"], "b-retained");
    assert_eq!(body["cursor"]["complete"], true);
}

#[tokio::test]
async fn worker_skips_deleted_objects_in_every_mode() {
    // Given a listing with one deleted object and one surviving object.
    for mode in [
        types::Mode::DryRun,
        types::Mode::Execute,
        types::Mode::Verify,
    ] {
        let state = fixture(false).await;
        let (keys, _) = state.storage.list_keys(None, 10).await.unwrap();
        let mut progress = progress::Progress::default();
        // When the worker processes the same page it was given by storage.
        for key in &keys {
            worker::process_key(&state, mode, key, &mut progress)
                .await
                .unwrap();
        }
        // Then the missing object is counted and the survivor is processed normally.
        let result = serde_json::to_value(progress).unwrap();
        assert_eq!(result["missing"], 1);
        assert_eq!(result["missing_keys"], json!(["a-deleted"]));
        assert_eq!(result["processed"], 2);
        match mode {
            types::Mode::DryRun => assert_eq!(result["by_key_id"], json!({"current":1})),
            types::Mode::Execute => assert_eq!(result["already_current"], 1),
            types::Mode::Verify => {
                assert_eq!(result["verified"], 1);
                assert_eq!(result["stale"], 0);
                assert_eq!(result["undecryptable"], 0);
            }
        }
    }
}

#[tokio::test]
async fn worker_counts_persistent_object_errors_and_continues_in_every_mode() {
    // Given an adapter returning access denied rather than a missing object.
    for mode in [
        types::Mode::DryRun,
        types::Mode::Execute,
        types::Mode::Verify,
    ] {
        let state = fixture(true).await;
        let mut progress = progress::Progress::default();
        // When the worker attempts the object operation.
        let outcome = worker::process_key(&state, mode, "b-retained", &mut progress).await;
        // Then the object failure is counted and its cursor can advance.
        assert!(outcome.is_ok());
        let progress = serde_json::to_value(progress).unwrap();
        assert_eq!(progress["processed"], 1);
        assert_eq!(progress["errors"], 1);
        assert_eq!(progress["error_samples"][0]["class"], "storage_error");
    }
}

#[test]
fn missing_samples_are_bounded_and_survive_resumption() {
    // Given more missing objects than the sample limit and legacy persisted progress.
    let mut progress: progress::Progress = serde_json::from_value(json!({"processed": 3})).unwrap();
    for number in 0..110 {
        progress.record_missing(&format!("workspace/{number}"));
    }
    // When restoring persisted progress.
    let restored: progress::Progress =
        serde_json::from_value(serde_json::to_value(progress).unwrap()).unwrap();
    // Then every missing object is counted and only the bounded sample remains.
    assert_eq!(
        (
            restored.missing,
            restored.missing_keys.len(),
            restored.processed
        ),
        (110, 100, 3)
    );
}
