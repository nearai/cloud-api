//! E2E tests for credit events and promo code management.
//!
//! Tests the full lifecycle: create event, generate codes, claim credits,
//! list events, deactivate events, and expiry filtering.

use crate::common::*;

static MOCK_USER_AGENT: &str = services::auth::ports::MOCK_USER_AGENT;

fn unique_name(prefix: &str) -> String {
    format!(
        "{}_{}",
        prefix,
        uuid::Uuid::new_v4().to_string().replace('-', "")
    )
}

/// Create a credit event via admin endpoint, return parsed response.
async fn create_credit_event(
    server: &axum_test::TestServer,
    request: &serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post("/v1/admin/credit-events")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(request)
        .await
}

/// Generate promo codes for a credit event via admin endpoint.
async fn generate_codes(
    server: &axum_test::TestServer,
    event_id: &str,
    count: i32,
) -> axum_test::TestResponse {
    server
        .post(format!("/v1/admin/credit-events/{event_id}/codes").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "count": count }))
        .await
}

/// Claim credits with a promo code (session-authenticated).
async fn claim_credits(
    server: &axum_test::TestServer,
    event_id: &str,
    code: &str,
) -> axum_test::TestResponse {
    server
        .post(format!("/v1/credit-events/{event_id}/claim").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "code": code }))
        .await
}

/// List active credit events (public).
async fn list_credit_events(server: &axum_test::TestServer) -> axum_test::TestResponse {
    server
        .get("/v1/credit-events")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

/// Get a credit event by ID (public).
async fn get_credit_event(
    server: &axum_test::TestServer,
    event_id: &str,
) -> axum_test::TestResponse {
    server
        .get(format!("/v1/credit-events/{event_id}").as_str())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

/// List promo codes for a credit event (admin).
async fn list_codes(server: &axum_test::TestServer, event_id: &str) -> axum_test::TestResponse {
    server
        .get(format!("/v1/admin/credit-events/{event_id}/codes").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

/// Deactivate a credit event (admin).
async fn deactivate_credit_event(
    server: &axum_test::TestServer,
    event_id: &str,
) -> axum_test::TestResponse {
    server
        .patch(format!("/v1/admin/credit-events/{event_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

fn future_iso_time(hours_from_now: i64) -> String {
    let dt = chrono::Utc::now() + chrono::Duration::hours(hours_from_now);
    dt.to_rfc3339()
}

fn past_iso_time(hours_ago: i64) -> String {
    let dt = chrono::Utc::now() - chrono::Duration::hours(hours_ago);
    dt.to_rfc3339()
}

/// Test creating a credit event with all fields.
#[tokio::test]
async fn test_create_credit_event() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("test_event"),
        "description": "Test event for E2E",
        "creditAmount": 10_000_000_000i64,
        "currency": "USD",
        "maxClaims": 100,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let response = create_credit_event(&server, &request).await;
    assert_eq!(response.status_code(), 200);

    let body: serde_json::Value = response.json();
    assert_eq!(body["currency"].as_str().unwrap(), "USD");
    assert_eq!(body["creditAmount"].as_i64().unwrap(), 10_000_000_000);
    assert_eq!(body["isActive"].as_bool().unwrap(), true);
    assert!(body["id"].as_str().is_some());
}

/// Test creating a credit event with minimal fields.
#[tokio::test]
async fn test_create_credit_event_minimal() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("minimal_event"),
        "creditAmount": 5_000_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let response = create_credit_event(&server, &request).await;
    assert_eq!(response.status_code(), 200);

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["name"].as_str().unwrap().starts_with("minimal_event_"),
        true
    );
    assert_eq!(body["creditAmount"].as_i64().unwrap(), 5_000_000_000);
}

/// Test creating a credit event with zero credits returns an error.
#[tokio::test]
async fn test_create_credit_event_zero_amount() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("zero_event"),
        "creditAmount": 0i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let response = create_credit_event(&server, &request).await;
    assert!(response.status_code() == 400 || response.status_code() == 422);
}

/// Test listing active credit events.
#[tokio::test]
async fn test_list_credit_events() {
    let server = setup_test_server().await;

    // Create two events
    for i in 0..2 {
        let request = serde_json::json!({
            "name": unique_name(&format!("list_event_{i}")),
            "creditAmount": 1_000_000_000i64,
            "creditExpiresAt": future_iso_time(720)
        });
        let response = create_credit_event(&server, &request).await;
        assert_eq!(response.status_code(), 200);
    }

    let response = list_credit_events(&server).await;
    assert_eq!(response.status_code(), 200);

    let body: serde_json::Value = response.json();
    let events = body.as_array().unwrap();
    assert!(events.len() >= 2);
}

/// Test getting a credit event by ID.
#[tokio::test]
async fn test_get_credit_event_by_id() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("get_event"),
        "description": "Get by ID test",
        "creditAmount": 7_500_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &request).await;
    assert_eq!(create_response.status_code(), 200);
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let get_response = get_credit_event(&server, event_id).await;
    assert_eq!(get_response.status_code(), 200);

    let got: serde_json::Value = get_response.json();
    assert_eq!(got["id"].as_str().unwrap(), event_id);
    assert_eq!(
        got["name"].as_str().unwrap(),
        created["name"].as_str().unwrap()
    );
}

/// Test deactivating a credit event.
#[tokio::test]
async fn test_deactivate_credit_event() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("deactivate_event"),
        "creditAmount": 3_000_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &request).await;
    assert_eq!(create_response.status_code(), 200);
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let deactivate_response = deactivate_credit_event(&server, event_id).await;
    assert_eq!(deactivate_response.status_code(), 200);

    let deactivated: serde_json::Value = deactivate_response.json();
    assert_eq!(deactivated["isActive"].as_bool().unwrap(), false);
}

/// Test generating promo codes for a credit event.
#[tokio::test]
async fn test_generate_promo_codes() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("codes_event"),
        "creditAmount": 5_000_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &request).await;
    assert_eq!(create_response.status_code(), 200);
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let codes_response = generate_codes(&server, event_id, 5).await;
    assert_eq!(codes_response.status_code(), 200);

    let codes_body: serde_json::Value = codes_response.json();
    let codes = codes_body["codes"].as_array().unwrap();
    assert_eq!(codes.len(), 5);

    // Verify codes start with "NEAR-"
    for code in codes {
        assert!(code.as_str().unwrap().starts_with("NEAR-"));
    }
}

/// Test listing promo codes for a credit event.
#[tokio::test]
async fn test_list_promo_codes() {
    let server = setup_test_server().await;

    let request = serde_json::json!({
        "name": unique_name("list_codes_event"),
        "creditAmount": 5_000_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    // Generate 3 codes
    let _ = generate_codes(&server, event_id, 3).await;

    let list_response = list_codes(&server, event_id).await;
    assert_eq!(list_response.status_code(), 200);

    let codes: serde_json::Value = list_response.json();
    let codes_arr = codes.as_array().unwrap();
    assert_eq!(codes_arr.len(), 3);

    // All should be unclaimed
    for code in codes_arr {
        assert_eq!(code["isClaimed"].as_bool().unwrap(), false);
    }
}

/// Test full claim flow: create event, generate codes, claim credits.
#[tokio::test]
async fn test_claim_credits_flow() {
    let server = setup_test_server().await;

    // Create event
    let event_request = serde_json::json!({
        "name": unique_name("claim_event"),
        "creditAmount": 10_000_000_000i64,
        "currency": "USD",
        "maxClaims": 10,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    assert_eq!(create_response.status_code(), 200);
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    // Generate codes
    let codes_response = generate_codes(&server, event_id, 1).await;
    assert_eq!(codes_response.status_code(), 200);
    let codes_body: serde_json::Value = codes_response.json();
    let code = codes_body["codes"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();

    // Claim credits
    let claim_response = claim_credits(&server, event_id, &code).await;
    assert_eq!(claim_response.status_code(), 200);

    let claim_body: serde_json::Value = claim_response.json();
    assert_eq!(claim_body["eventId"].as_str().unwrap(), event_id);
    assert_eq!(claim_body["creditAmount"].as_i64().unwrap(), 10_000_000_000);
    assert!(claim_body["organizationId"].as_str().is_some());
    assert!(claim_body["creditExpiresAt"].as_str().is_some());
}

/// Test claiming the same code twice returns an error.
#[tokio::test]
async fn test_claim_code_twice_fails() {
    let server = setup_test_server().await;

    let event_request = serde_json::json!({
        "name": unique_name("double_claim"),
        "creditAmount": 5_000_000_000i64,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let codes_response = generate_codes(&server, event_id, 1).await;
    let codes_body: serde_json::Value = codes_response.json();
    let code = codes_body["codes"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();

    // First claim should succeed
    let claim1 = claim_credits(&server, event_id, &code).await;
    assert_eq!(claim1.status_code(), 200);

    // Second claim of the same code should fail
    // Note: find_unclaimed_code filters is_claimed=false, so re-claiming
    // returns InvalidCode (404) rather than CodeAlreadyClaimed (409)
    let claim2 = claim_credits(&server, event_id, &code).await;
    let status = claim2.status_code();
    assert!(
        status == 404 || status == 409 || status == 400,
        "Expected 404, 409, or 400 for double claim, got {status}"
    );
}

/// Test claiming a non-existent code returns an error.
#[tokio::test]
async fn test_claim_invalid_code() {
    let server = setup_test_server().await;

    let event_request = serde_json::json!({
        "name": unique_name("invalid_code"),
        "creditAmount": 5_000_000_000i64,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let claim_response = claim_credits(&server, event_id, "NEAR-INVALIDCODE").await;
    assert_eq!(claim_response.status_code(), 404);
}

/// Test claiming credits on an inactive event returns an error.
#[tokio::test]
async fn test_claim_inactive_event() {
    let server = setup_test_server().await;

    let event_request = serde_json::json!({
        "name": unique_name("inactive_event"),
        "creditAmount": 5_000_000_000i64,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    // Deactivate the event
    let _ = deactivate_credit_event(&server, event_id).await;

    // Generate codes on inactive event (admin can still see)
    let codes_response = generate_codes(&server, event_id, 1).await;
    // Note: codes may still be generated even on inactive events
    // depending on business logic - but let's verify

    // Try to claim
    let codes_body: serde_json::Value = codes_response.json();
    if codes_body.get("codes").is_some() {
        let codes = codes_body["codes"].as_array().unwrap();
        if !codes.is_empty() {
            let code = codes[0].as_str().unwrap().to_string();
            let claim_response = claim_credits(&server, event_id, &code).await;
            assert!(
                claim_response.status_code() == 400,
                "Expected 400 for inactive event claim"
            );
        }
    }
}

/// Test that admin-only endpoints require authentication.
#[tokio::test]
async fn test_admin_endpoints_require_auth() {
    let server = setup_test_server().await;

    // Create without auth should fail
    let request = serde_json::json!({
        "name": unique_name("no_auth"),
        "creditAmount": 1_000_000_000i64,
        "creditExpiresAt": future_iso_time(720)
    });
    let response = server
        .post("/v1/admin/credit-events")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 401);

    // Generate codes without auth should fail
    let response = server
        .post("/v1/admin/credit-events/00000000-0000-0000-0000-000000000000/codes")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "count": 1 }))
        .await;
    assert_eq!(response.status_code(), 401);
}

/// Test that credit event claim updates claim count, and that
/// the same user cannot claim twice for the same event (per-user dedup).
#[tokio::test]
async fn test_claim_count_increments() {
    let server = setup_test_server().await;

    let event_request = serde_json::json!({
        "name": unique_name("claim_count"),
        "creditAmount": 1_000_000_000i64,
        "maxClaims": 10,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();
    assert_eq!(created["claimCount"].as_i64().unwrap(), 0);

    let codes_response = generate_codes(&server, event_id, 2).await;
    let codes_body: serde_json::Value = codes_response.json();
    let code1 = codes_body["codes"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();
    let code2 = codes_body["codes"].as_array().unwrap()[1]
        .as_str()
        .unwrap()
        .to_string();

    // Claim first code — should succeed and increment count to 1
    let claim1 = claim_credits(&server, event_id, &code1).await;
    assert_eq!(claim1.status_code(), 200);

    let get_response = get_credit_event(&server, event_id).await;
    let get_body: serde_json::Value = get_response.json();
    assert_eq!(get_body["claimCount"].as_i64().unwrap(), 1);

    // Claim second code with same user — should be rejected by per-user dedup
    let claim2 = claim_credits(&server, event_id, &code2).await;
    let status = claim2.status_code();
    assert!(
        status == 400 || status == 409,
        "Expected 400 or 409 for duplicate user claim, got {status}"
    );

    // Claim count should still be 1
    let get_response2 = get_credit_event(&server, event_id).await;
    let get_body2: serde_json::Value = get_response2.json();
    assert_eq!(get_body2["claimCount"].as_i64().unwrap(), 1);
}

/// Test that claiming respects max_claims limit.
#[tokio::test]
async fn test_max_claims_limit() {
    let server = setup_test_server().await;

    let event_request = serde_json::json!({
        "name": unique_name("max_claims"),
        "creditAmount": 1_000_000_000i64,
        "maxClaims": 1,
        "startsAt": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let codes_response = generate_codes(&server, event_id, 2).await;
    let codes_body: serde_json::Value = codes_response.json();
    let code1 = codes_body["codes"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();
    let code2 = codes_body["codes"].as_array().unwrap()[1]
        .as_str()
        .unwrap()
        .to_string();

    // First claim should succeed
    let claim1 = claim_credits(&server, event_id, &code1).await;
    assert_eq!(claim1.status_code(), 200);

    // Second claim should fail (max_claims reached)
    let claim2 = claim_credits(&server, event_id, &code2).await;
    assert!(
        claim2.status_code() == 400,
        "Expected 400 for max claims reached, got {}",
        claim2.status_code()
    );
}

/// Test credit expiry: expired credits should be filtered out from balance.
#[tokio::test]
async fn test_expired_credits_filtered_from_balance() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Add credits that expire in the past
    let credit_expires_at = past_iso_time(1);
    let response = server
        .patch(format!("/v1/admin/organizations/{}/limits", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "type": "grant",
            "spendLimit": { "amount": 10_000_000_000i64, "currency": "USD" },
            "creditExpiresAt": credit_expires_at,
            "changedBy": "test",
            "changeReason": "Testing expired credits"
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Check organization balance - expired credits should be filtered out
    // so balance spend_limit should be 0 (or absent/null)
    let balance_response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(balance_response.status_code(), 200);

    let balance: serde_json::Value = balance_response.json();
    // When credits expire, spend_limit becomes 0 (nano-dollars)
    let spend_limit = balance["spend_limit"].as_i64().unwrap_or(0);
    assert_eq!(
        spend_limit, 0,
        "Expired credits should not count toward spend_limit"
    );
}

/// Test that the claim endpoint requires authentication.
#[tokio::test]
async fn test_claim_requires_auth() {
    let server = setup_test_server().await;

    // Claim without auth should fail
    let response = server
        .post("/v1/credit-events/00000000-0000-0000-0000-000000000000/claim")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "code": "NEAR-TEST" }))
        .await;
    assert_eq!(response.status_code(), 401);
}

/// Test that credit events with claim deadline work correctly.
#[tokio::test]
async fn test_claim_deadline_enforcement() {
    let server = setup_test_server().await;

    // Create event with a past claim deadline
    let event_request = serde_json::json!({
        "name": unique_name("deadline_passed"),
        "creditAmount": 5_000_000_000i64,
        "startsAt": past_iso_time(2),
        "claimDeadline": past_iso_time(1),
        "creditExpiresAt": future_iso_time(720)
    });

    let create_response = create_credit_event(&server, &event_request).await;
    let created: serde_json::Value = create_response.json();
    let event_id = created["id"].as_str().unwrap();

    let codes_response = generate_codes(&server, event_id, 1).await;
    let codes_body: serde_json::Value = codes_response.json();
    let code = codes_body["codes"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();

    // Claim should fail because claim deadline has passed
    let claim_response = claim_credits(&server, event_id, &code).await;
    assert_eq!(claim_response.status_code(), 400);
}
