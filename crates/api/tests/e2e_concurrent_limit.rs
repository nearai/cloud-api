// E2E tests for organization concurrent request limit admin endpoints
mod common;

use common::*;
use services::auth::ports::MOCK_USER_AGENT;

/// Test getting concurrent limit for a new organization (should return null/default)
#[tokio::test]
async fn test_get_concurrent_limit_returns_default_for_new_org() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    let response = server
        .get(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let body: serde_json::Value = response.json();
    assert_eq!(body["organizationId"], org.id);
    assert!(
        body["concurrentLimit"].is_null(),
        "New org should have null concurrent limit (using default)"
    );
    assert_eq!(
        body["effectiveLimit"], 64,
        "Effective limit should be default 64"
    );
}

/// Test updating concurrent limit for an organization
#[tokio::test]
async fn test_update_concurrent_limit() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    // Update to a custom limit
    let update_response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": 128
        }))
        .await;

    assert_eq!(update_response.status_code(), 200);

    let update_body: serde_json::Value = update_response.json();
    assert_eq!(update_body["organizationId"], org.id);
    assert_eq!(update_body["concurrentLimit"], 128);

    // Verify the limit was persisted by getting it again
    let get_response = server
        .get(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(get_response.status_code(), 200);

    let get_body: serde_json::Value = get_response.json();
    assert_eq!(get_body["concurrentLimit"], 128);
    assert_eq!(
        get_body["effectiveLimit"], 128,
        "Effective limit should match custom limit"
    );
}

/// Test resetting concurrent limit to default (null)
#[tokio::test]
async fn test_reset_concurrent_limit_to_default() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    // First set a custom limit
    let set_response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": 256
        }))
        .await;
    assert_eq!(set_response.status_code(), 200);

    // Reset to default (null)
    let reset_response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": null
        }))
        .await;

    assert_eq!(reset_response.status_code(), 200);

    let reset_body: serde_json::Value = reset_response.json();
    assert!(
        reset_body["concurrentLimit"].is_null(),
        "Concurrent limit should be null after reset"
    );

    // Verify the reset was persisted
    let get_response = server
        .get(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(get_response.status_code(), 200);

    let get_body: serde_json::Value = get_response.json();
    assert!(
        get_body["concurrentLimit"].is_null(),
        "Concurrent limit should be null"
    );
    assert_eq!(
        get_body["effectiveLimit"], 64,
        "Effective limit should be back to default 64"
    );
}

/// Test that negative concurrent limit is rejected (deserialization error)
/// With u32 type, negative values fail at the deserialization layer
#[tokio::test]
async fn test_update_concurrent_limit_rejects_negative() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    let response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": -10
        }))
        .await;

    // Serde rejects negative values for u32 with 422 or 400
    assert!(
        response.status_code() == 400 || response.status_code() == 422,
        "Negative concurrent limit should be rejected with 400 or 422, got: {}",
        response.status_code()
    );
}

/// Test that zero concurrent limit is rejected
#[tokio::test]
async fn test_update_concurrent_limit_rejects_zero() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    let response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": 0
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Zero concurrent limit should be rejected"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "invalid_limits");
}

/// Test that non-existent organization returns 404
#[tokio::test]
async fn test_get_concurrent_limit_nonexistent_org() {
    let (server, _guard) = setup_test_server().await;
    let session_id = get_session_id();
    let fake_org_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/admin/organizations/{}/concurrent-limit", fake_org_id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Non-existent org should return 404"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "organization_not_found");
}

/// Test that updating non-existent organization returns 404
#[tokio::test]
async fn test_update_concurrent_limit_nonexistent_org() {
    let (server, _guard) = setup_test_server().await;
    let session_id = get_session_id();
    let fake_org_id = uuid::Uuid::new_v4();

    let response = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", fake_org_id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "concurrentLimit": 100
        }))
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Non-existent org should return 404"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "organization_not_found");
}

/// Test that invalid organization ID format returns 400
#[tokio::test]
async fn test_concurrent_limit_invalid_org_id_format() {
    let (server, _guard) = setup_test_server().await;
    let session_id = get_session_id();

    let response = server
        .get("/v1/admin/organizations/not-a-uuid/concurrent-limit")
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Invalid org ID format should return 400"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "invalid_id");
}

/// Test updating concurrent limit multiple times
#[tokio::test]
async fn test_update_concurrent_limit_multiple_times() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;
    let session_id = get_session_id();

    // Update to 100
    let response1 = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "concurrentLimit": 100 }))
        .await;
    assert_eq!(response1.status_code(), 200);

    // Update to 200
    let response2 = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "concurrentLimit": 200 }))
        .await;
    assert_eq!(response2.status_code(), 200);

    // Update to 50
    let response3 = server
        .patch(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "concurrentLimit": 50 }))
        .await;
    assert_eq!(response3.status_code(), 200);

    // Verify final value
    let get_response = server
        .get(format!("/v1/admin/organizations/{}/concurrent-limit", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(get_response.status_code(), 200);

    let body: serde_json::Value = get_response.json();
    assert_eq!(body["concurrentLimit"], 50);
    assert_eq!(body["effectiveLimit"], 50);
}
