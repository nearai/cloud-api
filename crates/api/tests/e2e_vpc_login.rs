mod common;

use api::routes::auth_vpc::VpcLoginResponse;
use common::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fs;
use std::path::PathBuf;

// ============================================
// VPC Login Tests
// ============================================

/// Guard type that ensures the VPC secret file is cleaned up when the test finishes
struct VpcSecretFileGuard {
    file_path: PathBuf,
}

impl Drop for VpcSecretFileGuard {
    fn drop(&mut self) {
        // Try to remove the file, but don't panic if it fails (e.g., already removed)
        let _ = fs::remove_file(&self.file_path);
    }
}

/// Set up VPC shared secret for testing by creating a temporary file
/// Returns a guard that must be kept alive for the test duration.
/// The file will be automatically cleaned up when the guard is dropped (when the test finishes).
fn setup_vpc_shared_secret(secret: &str) -> VpcSecretFileGuard {
    // Create a unique temporary file for this test
    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join(format!(
        "vpc_shared_secret_test_{}.txt",
        uuid::Uuid::new_v4()
    ));

    // Write the secret to the file
    fs::write(&file_path, secret).expect("Failed to write VPC shared secret to temp file");

    // Set the environment variable to point to the file
    std::env::set_var("VPC_SHARED_SECRET_FILE", file_path.to_str().unwrap());

    VpcSecretFileGuard { file_path }
}

/// Generate a valid VPC signature for testing
fn generate_vpc_signature(timestamp: i64, secret: &str) -> String {
    let message = timestamp.to_string();
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

#[tokio::test]
async fn test_vpc_login_success() {
    // Set the VPC shared secret for this test
    // The guard must be kept alive for the test duration to ensure cleanup happens after the test
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // VPC login requires user to have an organization, create one for the mock user
    create_org(&server).await;

    let timestamp = chrono::Utc::now().timestamp();
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "test-vpc-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        200,
        "VPC login should succeed with valid signature. Response: {:?}",
        response.text()
    );

    let body = response.json::<VpcLoginResponse>();

    assert!(
        !body.access_token.is_empty(),
        "Response should contain access_token"
    );
    assert!(
        !body.refresh_token.is_empty(),
        "Response should contain refresh_token"
    );
    assert!(!body.api_key.is_empty(), "Response should contain api_key");
    assert!(
        !body.organization.id.to_string().is_empty(),
        "Response should contain organization"
    );
    assert!(
        !body.workspace.id.to_string().is_empty(),
        "Response should contain workspace"
    );

    println!("✅ VPC login succeeded with valid signature");
}

#[tokio::test]
async fn test_vpc_login_expired_timestamp() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // Use timestamp from 5 minutes ago (beyond 30 second window)
    let timestamp = chrono::Utc::now().timestamp() - 300;
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "test-vpc-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        401,
        "VPC login should fail with expired timestamp"
    );

    println!("✅ Correctly rejected expired timestamp");
}

#[tokio::test]
async fn test_vpc_login_future_timestamp() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // Use timestamp 5 minutes in the future (beyond 30 second window)
    let timestamp = chrono::Utc::now().timestamp() + 300;
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "test-vpc-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        401,
        "VPC login should fail with future timestamp"
    );

    println!("✅ Correctly rejected future timestamp");
}

#[tokio::test]
async fn test_vpc_login_invalid_signature() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    let timestamp = chrono::Utc::now().timestamp();
    // Use wrong secret to generate signature
    let signature = generate_vpc_signature(timestamp, "wrong_secret");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "test-vpc-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        401,
        "VPC login should fail with invalid signature"
    );

    println!("✅ Correctly rejected invalid signature");
}

#[tokio::test]
async fn test_vpc_login_invalid_hex_signature() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    let timestamp = chrono::Utc::now().timestamp();

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": "not_valid_hex_zzz",
        "client_id": "test-vpc-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        401,
        "VPC login should fail with invalid hex signature"
    );

    println!("✅ Correctly rejected invalid hex signature");
}

#[tokio::test]
async fn test_vpc_login_creates_user_and_resources() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // VPC login requires user to have an organization, create one for the mock user
    create_org(&server).await;

    let client_id = format!("vpc-client-{}", uuid::Uuid::new_v4());
    let timestamp = chrono::Utc::now().timestamp();
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": client_id
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(response.status_code(), 200);

    let body = response.json::<VpcLoginResponse>();

    // Verify organization was created/returned
    assert!(
        !body.organization.id.to_string().is_empty(),
        "Organization should have id"
    );
    assert!(
        !body.organization.name.is_empty(),
        "Organization should have name"
    );

    // Verify workspace was created/returned
    assert!(
        !body.workspace.id.to_string().is_empty(),
        "Workspace should have id"
    );
    assert!(
        !body.workspace.name.is_empty(),
        "Workspace should have name"
    );

    // Verify API key is non-empty
    assert!(!body.api_key.is_empty(), "API key should not be empty");

    println!("✅ VPC login correctly creates user and resources");
}

#[tokio::test]
async fn test_vpc_login_api_key_works() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // VPC login requires user to have an organization, create one for the mock user
    create_org(&server).await;

    let timestamp = chrono::Utc::now().timestamp();
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "api-key-test-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(response.status_code(), 200);

    let body = response.json::<VpcLoginResponse>();

    // Try to use the API key to list models
    let models_response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", body.api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        models_response.status_code(),
        200,
        "API key from VPC login should work for authenticated requests"
    );

    println!("✅ API key from VPC login works correctly");
}

#[tokio::test]
async fn test_vpc_login_access_token_works() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // VPC login requires user to have an organization, create one for the mock user
    create_org(&server).await;

    let timestamp = chrono::Utc::now().timestamp();
    let signature = generate_vpc_signature(timestamp, "test_vpc_secret_123");

    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": signature,
        "client_id": "access-token-test-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(response.status_code(), 200);

    let body = response.json::<VpcLoginResponse>();

    // Try to use the access token to get user info
    let user_response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {}", body.access_token))
        .await;

    assert_eq!(
        user_response.status_code(),
        200,
        "Access token from VPC login should work for authenticated requests"
    );

    // Note: In mock auth mode, the mock service returns the mock user, not the actual VPC user.
    // The important thing is that the access token is accepted (200 response).
    let user = user_response.json::<api::models::UserResponse>();
    assert!(!user.id.is_empty(), "User should have an id");
    assert!(!user.email.is_empty(), "User should have an email");

    println!("✅ Access token from VPC login works correctly");
}

#[tokio::test]
async fn test_vpc_login_missing_fields() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let (server, _guard) = setup_test_server().await;

    // Missing signature
    let request = serde_json::json!({
        "timestamp": chrono::Utc::now().timestamp(),
        "client_id": "test-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        422,
        "Should fail with missing signature field"
    );

    // Missing timestamp
    let request = serde_json::json!({
        "signature": "abc123",
        "client_id": "test-client"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        422,
        "Should fail with missing timestamp field"
    );

    // Missing client_id
    let request = serde_json::json!({
        "timestamp": chrono::Utc::now().timestamp(),
        "signature": "abc123"
    });

    let response = server.post("/v1/auth/vpc/login").json(&request).await;

    assert_eq!(
        response.status_code(),
        422,
        "Should fail with missing client_id field"
    );

    println!("✅ Correctly rejected requests with missing fields");
}
