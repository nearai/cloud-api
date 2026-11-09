use super::*;
use async_trait::async_trait;
use services::files::{
    storage::{ObjectMetadata, ObjectVerification, RewrapOutcome},
    FileServiceError,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn server(state: ObjectEncryptionState) -> TestServer {
    TestServer::builder()
        .mock_transport()
        .build(router().layer(Extension(state)).layer(Extension(admin())))
}

#[tokio::test]
async fn rewrap_gate_reports_false_and_refuses_execute_without_affecting_uploads() {
    // Given a disabled rewrap gate with a functional upload adapter.
    let ring = Arc::new(ObjectKeyRing::new("current".into(), [9; 32], vec![]).unwrap());
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    let server = server(
        ObjectEncryptionState::new(DbPool::uninitialized(), storage.clone(), ring)
            .with_job_options(256, false),
    );
    storage
        .upload("object", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // When querying keys and attempting an execute job.
    let keys = server.get("/admin/object-encryption/keys").await;
    let response = server
        .post("/admin/object-encryption/jobs")
        .json(&json!({"mode":"execute"}))
        .await;
    // Then the gate is accurately reported and the job is refused before database access.
    assert_eq!(keys.json::<Value>()["write_enabled"], false);
    response.assert_status_bad_request();
    assert_eq!(storage.download("object").await.unwrap(), b"content");
    server
        .post("/admin/object-encryption/scan")
        .json(&json!({}))
        .await
        .assert_status_ok();
}

struct SlowMetadata {
    inner: Arc<MockStorage>,
    stall: AtomicBool,
    list_calls: AtomicUsize,
    metadata_calls: AtomicUsize,
}

#[async_trait]
impl StorageTrait for SlowMetadata {
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
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_keys(continuation, max).await
    }
    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        self.metadata_calls.fetch_add(1, Ordering::SeqCst);
        if key == "b" && self.stall.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
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

#[tokio::test]
async fn scan_deadline_returns_the_unprocessed_page_and_resumes_it() {
    // Given a page whose second metadata operation never completes.
    let ring = Arc::new(ObjectKeyRing::new("current".into(), [9; 32], vec![]).unwrap());
    let inner = Arc::new(MockStorage::new_with_ring(ring.clone()));
    for key in ["a", "b", "c"] {
        inner.upload(key, vec![], "text/plain").await.unwrap();
    }
    let storage = Arc::new(SlowMetadata {
        inner,
        stall: AtomicBool::new(true),
        list_calls: AtomicUsize::new(0),
        metadata_calls: AtomicUsize::new(0),
    });
    let server = server(ObjectEncryptionState::new(
        DbPool::uninitialized(),
        storage.clone(),
        ring,
    ));
    // When the one-second whole-scan deadline interrupts its current page.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        server
            .post("/admin/object-encryption/scan")
            .json(&json!({"deadline_seconds":1})),
    )
    .await
    .unwrap();
    // Then the completed object is counted and every unprocessed key remains resumable.
    response.assert_status_ok();
    let first = response.json::<Value>();
    assert_eq!(first["complete"], false);
    assert_eq!(first["totals"]["processed"], 1);
    assert_eq!(first["cursor"]["state"]["pending_keys"], json!(["b", "c"]));
    storage.stall.store(false, Ordering::SeqCst);
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"cursor":first["cursor"]}))
        .await;
    response.assert_status_ok();
    let second = response.json::<Value>();
    assert_eq!(second["complete"], true);
    assert_eq!(second["totals"]["processed"], 2);
    assert_eq!(second["cursor"]["state"]["last_key"], "c");
}

#[tokio::test]
async fn scan_rejects_tampered_pending_keys_before_storage_access() {
    // Given a genuine deadline cursor containing a pending object from a listed page.
    let ring = Arc::new(ObjectKeyRing::new("current".into(), [9; 32], vec![]).unwrap());
    let inner = Arc::new(MockStorage::new_with_ring(ring.clone()));
    for key in ["a", "b", "c"] {
        inner.upload(key, vec![], "text/plain").await.unwrap();
    }
    let storage = Arc::new(SlowMetadata {
        inner,
        stall: AtomicBool::new(true),
        list_calls: AtomicUsize::new(0),
        metadata_calls: AtomicUsize::new(0),
    });
    let server = server(ObjectEncryptionState::new(
        DbPool::uninitialized(),
        storage.clone(),
        ring,
    ));
    let first = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"deadline_seconds":1}))
        .await
        .json::<Value>();
    let mut cursor = first["cursor"].clone();
    cursor["state"]["pending_keys"] = json!(["attacker-controlled-object"]);
    let list_calls = storage.list_calls.load(Ordering::SeqCst);
    let metadata_calls = storage.metadata_calls.load(Ordering::SeqCst);

    // When the caller changes the pending object name and resumes the scan.
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"cursor":cursor}))
        .await;

    // Then authentication rejects the cursor before either storage operation runs.
    response.assert_status_bad_request();
    assert_eq!(storage.list_calls.load(Ordering::SeqCst), list_calls);
    assert_eq!(
        storage.metadata_calls.load(Ordering::SeqCst),
        metadata_calls
    );

    for (field, value) in [
        ("continuation", json!("attacker-controlled-continuation")),
        ("last_key", json!("attacker-controlled-last-key")),
        ("complete", json!(true)),
    ] {
        let mut altered = first["cursor"].clone();
        altered["state"][field] = value;
        let response = server
            .post("/admin/object-encryption/scan")
            .json(&json!({"cursor":altered}))
            .await;
        response.assert_status_bad_request();
        assert_eq!(storage.list_calls.load(Ordering::SeqCst), list_calls);
        assert_eq!(
            storage.metadata_calls.load(Ordering::SeqCst),
            metadata_calls
        );
    }

    let mut unknown_key = first["cursor"].clone();
    unknown_key["key_id"] = json!("unknown");
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"cursor":unknown_key}))
        .await;
    response.assert_status_bad_request();
    assert_eq!(storage.list_calls.load(Ordering::SeqCst), list_calls);
    assert_eq!(
        storage.metadata_calls.load(Ordering::SeqCst),
        metadata_calls
    );

    let mut malformed_tag = first["cursor"].clone();
    malformed_tag["tag"] = json!("not-hex");
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"cursor":malformed_tag}))
        .await;
    response.assert_status_bad_request();
    assert_eq!(storage.list_calls.load(Ordering::SeqCst), list_calls);
    assert_eq!(
        storage.metadata_calls.load(Ordering::SeqCst),
        metadata_calls
    );

    let mut unsigned = first["cursor"].clone();
    unsigned.as_object_mut().unwrap().remove("tag");
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"cursor":unsigned}))
        .await;
    response.assert_status_unprocessable_entity();
    assert_eq!(storage.list_calls.load(Ordering::SeqCst), list_calls);
    assert_eq!(
        storage.metadata_calls.load(Ordering::SeqCst),
        metadata_calls
    );
}

#[tokio::test]
async fn scan_defaults_to_ten_thousand_objects() {
    // Given one more object than the default call budget.
    let (server, storage) = fixture();
    for index in 0..10_001 {
        storage
            .upload(&format!("object-{index:05}"), vec![], "text/plain")
            .await
            .unwrap();
    }
    // When the caller supplies no bounds.
    let result = server
        .post("/admin/object-encryption/scan")
        .json(&json!({}))
        .await
        .json::<Value>();
    // Then scan stops at the documented default with a continuation.
    assert_eq!(result["totals"]["processed"], 10_000);
    assert_eq!(result["complete"], false);
    assert_eq!(result["cursor"]["state"]["last_key"], "object-09999");
}

#[test]
fn response_samples_are_capped_without_losing_counts_or_exposing_lease() {
    // Given internal persisted progress larger than the administrator response sample cap.
    let mut progress = json!({"missing":100,"changed":101,"errors":102,"oversized":103,"undecryptable":104,"unknown_key_ids_warned":true});
    for field in [
        "missing_keys",
        "changed_keys",
        "error_samples",
        "oversized_keys",
        "undecryptable_keys",
    ] {
        progress[field] = json!(vec!["object"; 100]);
    }
    // When serializing public job progress and cursor.
    let result = endpoints::public_progress(progress);
    let cursor = endpoints::public_cursor(
        json!({"lease":{"token":"private"},"pending_keys":["object"],"pending_next":"private","complete":false}),
    );
    // Then all totals survive, each sample is bounded, and worker internals stay private.
    assert_eq!(result["missing"], 100);
    assert_eq!(result["errors"], 102);
    for field in [
        "missing_keys",
        "changed_keys",
        "error_samples",
        "oversized_keys",
        "undecryptable_keys",
    ] {
        assert_eq!(result[field].as_array().unwrap().len(), 20);
    }
    assert!(result.get("unknown_key_ids_warned").is_none());
    assert_eq!(cursor, json!({"complete":false}));
}
