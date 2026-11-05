use super::*;
use crate::middleware::AdminUser;
use axum::{http::StatusCode, Extension};
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
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), storage.clone(), ring);
    let app = router().layer(Extension(state)).layer(Extension(admin()));
    (TestServer::builder().mock_transport().build(app), storage)
}

pub(super) fn admin() -> AdminUser {
    let now = chrono::Utc::now();
    AdminUser(database::User {
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
    })
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
        json!({"mode":"execute","batch_delay_ms":0,"scope":{"options":{"batch_delay_ms":10}}}),
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

#[tokio::test]
async fn scan_groups_unconfigured_metadata_into_unknown_without_trusting_it() {
    // Given test-local objects whose metadata names unconfigured IDs.
    let (server, storage) = fixture();
    for id in ["retired", "arbitrary-untrusted-id"] {
        storage
            .seed_raw(
                id,
                encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                Some(id.into()),
                "text/plain",
            )
            .unwrap();
    }
    // When scanning via the real object route.
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({}))
        .await;
    // Then unconfigured names cannot become inventory map keys.
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>()["totals"]["by_key_id"],
        json!({"unknown":2})
    );
    assert_eq!(storage.download("retired").await.unwrap(), b"legacy");
}

#[tokio::test]
async fn scan_continues_at_the_returned_cursor_without_repeating_objects() {
    // Given two objects and the first bounded scan result.
    let (server, storage) = fixture();
    for key in ["a", "b"] {
        storage.upload(key, vec![], "text/plain").await.unwrap();
    }
    let first = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"limit":1}))
        .await
        .json::<Value>();
    // When the caller continues with the returned cursor.
    let response = server
        .post("/admin/object-encryption/scan")
        .json(&json!({"limit":1,"cursor":first["cursor"]}))
        .await;
    // Then only the second object is inventoried and the scan completes.
    response.assert_status_ok();
    let result = response.json::<Value>();
    assert_eq!(result["cursor"]["last_key"], "b");
    assert_eq!(result["complete"], true);
    assert_eq!(result["totals"]["processed"], 1);
}

#[tokio::test]
async fn scan_rejects_invalid_object_and_wall_clock_bounds() {
    // Given an in-process admin route with empty storage.
    let (server, _) = fixture();
    for body in [
        json!({"limit":100001}),
        json!({"limit":0}),
        json!({"deadline_seconds":0}),
        json!({"deadline_seconds":61}),
    ] {
        // When the caller requests a bound outside the supported range.
        let response = server
            .post("/admin/object-encryption/scan")
            .json(&body)
            .await;
        // Then validation rejects it before storage work.
        response.assert_status_bad_request();
    }
}

#[path = "http_scan_tests.rs"]
mod scan_tests;

#[tokio::test]
async fn request_field_misspellings_are_rejected_by_the_json_extractor() {
    // Given the real router with no initialized database connections.
    let (server, _) = fixture();
    let mut statuses = Vec::new();
    for (route, body) in [
        ("jobs", json!({"mode":"execute","batch_szie":1})),
        ("verify", json!({"batch_szie":1})),
        ("scan", json!({"deadline_secods":1})),
    ] {
        // When a caller misspells an optional field at the request boundary.
        let response = server
            .post(&format!("/admin/object-encryption/{route}"))
            .json(&body)
            .await;
        statuses.push((route, response.status_code()));
    }
    // Then every request fails extraction before handler side effects.
    assert_eq!(
        statuses,
        ["jobs", "verify", "scan"].map(|route| (route, StatusCode::UNPROCESSABLE_ENTITY))
    );
}
