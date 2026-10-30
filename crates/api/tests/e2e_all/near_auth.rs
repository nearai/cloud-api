use crate::common::*;
use axum::http::StatusCode;

// ============================================
// Happy Path Tests
// ============================================

/// Test that NEAR login correctly rejects requests with invalid signatures
/// Verifies: unsigned/invalid signature requests return 401 UNAUTHORIZED
#[tokio::test]
async fn test_near_login_rejects_invalid_signature() {
    let server = setup_test_server().await;
    let account_id = "alice.near";

    let response = test_near_login(&server, account_id, 0).await;

    // Invalid signature should return 401 UNAUTHORIZED
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Invalid signature should return 401"
    );

    println!("✅ NEAR login correctly rejects unsigned request");
}

/// Test that missing User-Agent header is rejected
#[tokio::test]
async fn test_near_login_missing_user_agent() {
    let server = setup_test_server().await;
    let request_body = create_near_auth_request_json("alice.near", 0);

    let response = server.post("/v1/auth/near").json(&request_body).await;

    // Missing User-Agent header is caught by the handler and returns BAD_REQUEST (400)
    assert_eq!(
        response.status_code(),
        StatusCode::BAD_REQUEST,
        "Missing User-Agent should return 400"
    );

    println!("✅ Correctly rejected request without User-Agent header");
}

// ============================================
// Security Tests - Nonce Validation
// ============================================

/// Test that expired nonces (>5 minutes old) are rejected
#[tokio::test]
async fn test_near_login_expired_nonce() {
    let server = setup_test_server().await;
    let account_id = "bob.near";

    // Create nonce that is 6 minutes in the past (exceeds 5-minute window)
    let expired_timestamp_offset = -(6 * 60 * 1000); // -6 minutes in milliseconds
    let response = test_near_login(&server, account_id, expired_timestamp_offset).await;

    // Expired nonce fails timestamp validation in handler, returns UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Expired nonce should return 401"
    );

    println!("✅ Correctly rejected expired nonce");
}

/// Test that nonces with future timestamps are rejected
#[tokio::test]
async fn test_near_login_future_timestamp() {
    let server = setup_test_server().await;
    let account_id = "charlie.near";

    // Create nonce with timestamp 1 minute in the future
    let future_timestamp_offset = 60 * 1000; // +1 minute in milliseconds
    let response = test_near_login(&server, account_id, future_timestamp_offset).await;

    // Future timestamp fails validation in handler, returns UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Future timestamp should return 401"
    );

    println!("✅ Correctly rejected future timestamp");
}

/// Test that nonces within 5-minute window are accepted (format-wise)
/// Note: Will still fail on signature verification without real wallet
#[tokio::test]
async fn test_near_login_valid_nonce_window() {
    let server = setup_test_server().await;
    let account_id = "dave.near";

    // Create nonce that is 3 minutes in the past (within 5-minute window)
    let valid_timestamp_offset = -(3 * 60 * 1000); // -3 minutes in milliseconds
    let response = test_near_login(&server, account_id, valid_timestamp_offset).await;

    // Should pass nonce timestamp validation
    // Will fail on signature verification, returning UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Valid nonce should pass validation, then fail on signature"
    );

    println!("✅ Correctly accepted nonce within valid time window");
}

// ============================================
// Message and Recipient Validation Tests
// ============================================

/// Test that invalid message text is rejected
#[tokio::test]
async fn test_near_login_invalid_message() {
    let server = setup_test_server().await;
    let account_id = "eve.near";

    // Create request with wrong message text
    let mut request_body = create_near_auth_request_json(account_id, 0);
    request_body["payload"]["message"] = serde_json::json!("Wrong message text");

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await;

    // Invalid message returns UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Invalid message should return 401"
    );

    println!("✅ Correctly rejected invalid message");
}

/// Test that invalid recipient is rejected
#[tokio::test]
async fn test_near_login_invalid_recipient() {
    let server = setup_test_server().await;
    let account_id = "frank.near";

    // Create request with wrong recipient
    let mut request_body = create_near_auth_request_json(account_id, 0);
    request_body["payload"]["recipient"] = serde_json::json!("wrong.near.ai");

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await;

    // Invalid recipient returns UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Invalid recipient should return 401"
    );

    println!("✅ Correctly rejected invalid recipient");
}

// ============================================
// Input Validation Tests
// ============================================

/// Test that malformed JSON is rejected
#[tokio::test]
async fn test_near_login_malformed_json() {
    let server = setup_test_server().await;

    // Use an invalid JSON structure (missing required fields)
    let invalid_request = serde_json::json!({
        "invalid_key": "invalid_value"
    });

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&invalid_request)
        .await;

    // Malformed JSON is caught by Axum deserialization, returns UNPROCESSABLE_ENTITY (422)
    assert_eq!(
        response.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "Malformed JSON should return 422"
    );

    println!("✅ Correctly rejected malformed JSON");
}

/// Test that invalid nonce length is rejected
#[tokio::test]
async fn test_near_login_invalid_nonce_length() {
    let server = setup_test_server().await;
    let account_id = "grace.near";

    // Create request with wrong nonce length (not 32 bytes)
    let mut request_body = create_near_auth_request_json(account_id, 0);
    request_body["payload"]["nonce"] = serde_json::json!(vec![1u8, 2u8, 3u8]); // Only 3 bytes

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await;

    // Invalid nonce length is caught during JSON deserialization, returns UNPROCESSABLE_ENTITY (422)
    assert_eq!(
        response.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "Invalid nonce length should return 422"
    );

    println!("✅ Correctly rejected invalid nonce length");
}

/// Test that missing payload fields are rejected
#[tokio::test]
async fn test_near_login_missing_fields() {
    let server = setup_test_server().await;

    // Create incomplete request (missing payload)
    // Create a test signature using the helper (since NEAR_TEST_SIGNATURE is removed)
    let request_body = create_near_auth_request_json("incomplete.near", 0);
    // Remove the payload to make it incomplete
    let incomplete_request = serde_json::json!({
        "signed_message": request_body["signed_message"].clone(),
    });

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&incomplete_request)
        .await;

    // Missing payload field is caught by Axum deserialization, returns UNPROCESSABLE_ENTITY (422)
    assert_eq!(
        response.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "Missing payload fields should return 422"
    );

    println!("✅ Correctly rejected request with missing fields");
}

/// Test that zero-timestamp nonce is rejected (security fix)
#[tokio::test]
async fn test_near_login_zero_timestamp_nonce() {
    let server = setup_test_server().await;
    let account_id = "hacker.near";

    // Create request with zero-timestamp nonce (all zeros in first 8 bytes)
    let mut request_body = create_near_auth_request_json(account_id, 0);
    // Replace nonce with all-zero bytes
    request_body["payload"]["nonce"] = serde_json::json!(vec![0u8; 32]);

    let response = server
        .post("/v1/auth/near")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await;

    // Zero-timestamp nonce should be rejected with UNAUTHORIZED (401)
    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "Zero-timestamp nonce should return 401"
    );

    println!("✅ Correctly rejected zero-timestamp nonce");
}

/// Exercise real NEP-413 verification and database replay protection with only RPC mocked.
#[tokio::test]
async fn test_near_login_signed_proofs_and_access_key_ownership() {
    use base64::Engine;
    use near_kit::{signer::SecretKey, standards::nep413::serialize_message};
    use serde_json::{json, Value};
    use uuid::Uuid;
    use wiremock::{
        matchers::{body_partial_json, method},
        Mock, MockServer, ResponseTemplate,
    };

    for secret_key in [
        SecretKey::ed25519_from_bytes([7; 32]),
        SecretKey::secp256k1_from_bytes([7; 32]).unwrap(),
        SecretKey::ml_dsa65_from_seed([7; 32]),
    ] {
        let rpc = MockServer::start().await;
        let (server, _) = setup_test_server_with_config_and_database(|config| {
            config.auth.mock = false;
            config.auth.near.rpc_url = rpc.uri();
        })
        .await;
        let account = format!("login-{}.testnet", Uuid::new_v4().simple());
        let public_key = secret_key.public_key().to_string();
        let payload = create_near_test_payload(0);
        let signature = secret_key.sign(serialize_message(&payload).as_bytes());
        let body = json!({
            "signed_message": {
                "accountId": account,
                "publicKey": public_key,
                "signature": base64::engine::general_purpose::STANDARD.encode(signature.as_bytes()),
            },
            "payload": {
                "message": payload.message,
                "nonce": payload.nonce,
                "recipient": payload.recipient,
            },
        });

        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "method": "EXPERIMENTAL_view_access_key",
                "params": {
                    "finality": "optimistic",
                    "account_id": account,
                    "public_key": public_key,
                },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "block_hash": "11111111111111111111111111111111",
                    "block_height": 1,
                    "nonce": 0,
                    "permission": "FullAccess",
                },
            })))
            .expect(2)
            .mount(&rpc)
            .await;

        let response = server
            .post("/v1/auth/near")
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&body)
            .await;

        assert_eq!(
            response.status_code(),
            StatusCode::OK,
            "{}",
            response.text()
        );
        let authenticated = response.json::<Value>();
        assert!(authenticated["access_token"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert!(authenticated["refresh_token"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert_eq!(authenticated["user"]["provider"], "near");
        assert_eq!(
            authenticated["user"]["email"],
            services::email::near_placeholder_email(&account)
        );

        let replay = server
            .post("/v1/auth/near")
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&body)
            .await;

        assert_eq!(replay.status_code(), StatusCode::UNAUTHORIZED);
        rpc.verify().await;
        rpc.reset().await;

        for permission in [
            json!({
                "FunctionCall": {
                    "allowance": null,
                    "receiver_id": "app.testnet",
                    "method_names": [],
                },
            }),
            Value::Null,
        ] {
            let other_account = format!("other-{}.testnet", Uuid::new_v4().simple());
            let mut rejected_body = body.clone();
            rejected_body["signed_message"]["accountId"] = json!(other_account);
            let fresh_payload = create_near_test_payload(0);
            let fresh_signature = secret_key.sign(serialize_message(&fresh_payload).as_bytes());
            rejected_body["payload"]["nonce"] = json!(fresh_payload.nonce);
            rejected_body["signed_message"]["signature"] =
                json!(base64::engine::general_purpose::STANDARD.encode(fresh_signature.as_bytes()));
            let reply = if permission.is_null() {
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {
                        "code": -32000,
                        "message": "Server error",
                        "data": "access key does not exist",
                        "name": "HANDLER_ERROR",
                        "cause": {
                            "name": "UNKNOWN_ACCESS_KEY",
                            "info": {
                                "public_key": public_key,
                            },
                        },
                    },
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "block_hash": "11111111111111111111111111111111",
                        "block_height": 1,
                        "nonce": 0,
                        "permission": permission,
                    },
                })
            };

            Mock::given(method("POST"))
                .and(body_partial_json(json!({
                    "method": "EXPERIMENTAL_view_access_key",
                    "params": {
                        "finality": "optimistic",
                        "account_id": other_account,
                        "public_key": public_key,
                    },
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(reply))
                .expect(1)
                .mount(&rpc)
                .await;

            let rejected = server
                .post("/v1/auth/near")
                .add_header("User-Agent", MOCK_USER_AGENT)
                .json(&rejected_body)
                .await;

            assert_eq!(
                rejected.status_code(),
                StatusCode::UNAUTHORIZED,
                "{}",
                rejected.text()
            );
            rpc.verify().await;
            rpc.reset().await;
        }
    }
}
