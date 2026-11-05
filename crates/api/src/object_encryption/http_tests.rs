use super::*;
use crate::middleware::AdminUser;
use axum::Extension;
use axum_test::TestServer;
use serde_json::{json, Value};
use services::files::{encryption::encrypt_with_key, storage::MockStorage};

fn fixture() -> (TestServer, Arc<MockStorage>) {
    let ring = Arc::new(
        ObjectKeyRing::new(
            "current".into(),
            [9; 32],
            vec![("previous".into(), [7; 32])],
        )
        .unwrap(),
    );
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    let now = chrono::Utc::now();
    let admin = AdminUser(database::User {
        id: Uuid::new_v4(),
        email: "admin@near.ai".into(),
        username: "admin".into(),
        display_name: None,
        avatar_url: None,
        created_at: now,
        updated_at: now,
        last_login_at: None,
        is_active: true,
        auth_provider: "google".into(),
        provider_user_id: "admin".into(),
        tokens_revoked_at: None,
        contact_email: None,
        contact_email_verified_at: None,
        email_opt_in_at: None,
    });
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), storage.clone(), ring);
    let app = router().layer(Extension(state)).layer(Extension(admin));
    (TestServer::builder().mock_transport().build(app), storage)
}

#[tokio::test]
async fn scan_reports_metadata_and_preserves_the_objects() {
    // Given legacy and current objects in the real in-memory storage adapter.
    let (server, storage) = fixture();
    storage
        .seed_raw(
            "workspace/legacy",
            encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    storage
        .upload("workspace/current", b"current".to_vec(), "text/plain")
        .await
        .unwrap();
    // When the real scan route inventories them.
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({}))
        .await;
    // Then metadata counts are complete and the legacy object is unchanged.
    response.assert_status_ok();
    let body = response.json::<Value>();
    assert_eq!(body["totals"]["by_key_id"], json!({"none":1,"current":1}));
    assert_eq!(body["totals"]["total"], 2);
    assert_eq!(body["totals"]["complete"], true);
    assert_eq!(
        storage.object_key_id("workspace/legacy").await.unwrap(),
        None
    );
    assert_eq!(
        storage.download("workspace/legacy").await.unwrap(),
        b"legacy"
    );
}

#[tokio::test]
async fn scan_limit_reports_an_incomplete_inventory_without_skipping_the_boundary() {
    // Given two objects ordered by their storage keys.
    let (server, storage) = fixture();
    for key in ["workspace/a", "workspace/b"] {
        storage.upload(key, Vec::new(), "text/plain").await.unwrap();
    }
    // When a scan is limited to one object.
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"limit":1}))
        .await;
    // Then the returned cursor identifies the next page and completed key.
    response.assert_status_ok();
    let body = response.json::<Value>();
    assert_eq!(body["totals"]["total"], 1);
    assert_eq!(body["totals"]["complete"], false);
    assert_eq!(
        body["cursor"],
        json!({"continuation":"workspace/a","last_key":"workspace/a","complete":false})
    );
}

#[tokio::test]
async fn keys_reports_the_configured_read_order_and_safe_fingerprints() {
    // Given an object ring with one write and one previous key.
    let (server, _) = fixture();
    // When retrieving the ring through its real route.
    let response = server.get("/admin/object-encryption/keys").await;
    // Then IDs and fixed-length fingerprints describe the ring without key bytes.
    response.assert_status_ok();
    let body = response.json::<Value>();
    assert_eq!(body["write_key_id"], "current");
    assert_eq!(body["read_key_ids"], json!(["current", "previous"]));
    let fingerprints = body["fingerprints"].as_object().unwrap();
    assert_eq!(fingerprints.len(), 2);
    for fingerprint in fingerprints.values() {
        assert_eq!(fingerprint.as_str().unwrap().len(), 16);
    }
}

#[tokio::test]
async fn invalid_jobs_are_rejected_before_accessing_the_database() {
    // Given an uninitialized pool and invalid job requests.
    let (server, _) = fixture();
    for body in [
        json!({"mode":"execute","batch_size":0}),
        json!({"mode":"verify","actions":["rewrap"]}),
        json!({"mode":"execute","batch_delay_ms":10001}),
    ] {
        // When submitting a job to the real handler.
        let response = server
            .post("/admin/object-encryption/jobs")
            .json(&body)
            .await;
        // Then validation returns a client error rather than a pool failure.
        response.assert_status_bad_request();
        assert_eq!(response.json::<Value>()["error"]["code"], "invalid_request");
    }
}
