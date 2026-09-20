use crate::common::*;
use api::routes::auth_vpc::VpcLoginResponse;
use hmac::{Hmac, KeyInit, Mac};
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

    // Exercise real signup so the returned user has its persisted default
    // organization and the issued credentials belong to that VPC identity.
    let server = setup_test_server_with_config(|config| config.auth.mock = false).await;

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

    let server = setup_test_server().await;

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

    let server = setup_test_server().await;

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

    let server = setup_test_server().await;

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

    let server = setup_test_server().await;

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

    // Exercise real signup so the returned user has its persisted default
    // organization and the issued credentials belong to that VPC identity.
    let server = setup_test_server_with_config(|config| config.auth.mock = false).await;

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

    // Exercise real signup so the returned user has its persisted default
    // organization and the issued credentials belong to that VPC identity.
    let server = setup_test_server_with_config(|config| config.auth.mock = false).await;

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

    // Real authentication refreshes its API-key bloom filter every 10 seconds.
    // Wait for the new key to become visible, without masking other HTTP errors.
    let auth_response = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let response = server
                .get("/v1/files?limit=1")
                .add_header("Authorization", format!("Bearer {}", body.api_key))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            if response.status_code() != 401 {
                break response;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("VPC API key should become visible to real authentication");

    assert_eq!(
        auth_response.status_code(),
        200,
        "API key from VPC login should work for authenticated requests"
    );

    println!("✅ API key from VPC login works correctly");
}

#[tokio::test]
async fn test_vpc_login_access_token_works() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    // Exercise real signup so the returned user has its persisted default
    // organization and the issued credentials belong to that VPC identity.
    let server = setup_test_server_with_config(|config| config.auth.mock = false).await;

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

    let user = user_response.json::<api::models::UserResponse>();
    assert_eq!(user.id, body.session.user_id.to_string());
    assert_eq!(user.email, "access-token-test-client@vpc.internal.near.ai");
    assert_eq!(
        user.default_organization_id,
        Some(body.organization.id.to_string())
    );

    println!("✅ Access token from VPC login works correctly");
}

#[tokio::test]
async fn test_vpc_login_missing_fields() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");

    let server = setup_test_server().await;

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

#[tokio::test]
async fn test_vpc_login_mock_uses_persisted_default() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");
    let server = setup_test_server().await;
    create_org(&server).await;
    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await
        .json::<serde_json::Value>();
    let timestamp = chrono::Utc::now().timestamp();
    let response = server
        .post("/v1/auth/vpc/login")
        .json(&serde_json::json!({
            "timestamp": timestamp,
            "signature": generate_vpc_signature(timestamp, "test_vpc_secret_123"),
            "client_id": "mock-vpc-client"
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<VpcLoginResponse>();
    assert_eq!(
        body.organization.id.to_string(),
        me["default_organization_id"].as_str().unwrap()
    );
}

#[tokio::test]
async fn test_vpc_login_revoked_default_membership_is_forbidden() {
    let _guard = setup_vpc_shared_secret("test_vpc_secret_123");
    let (server, database) =
        setup_test_server_with_config_and_database(|config| config.auth.mock = false).await;
    let timestamp = chrono::Utc::now().timestamp();
    let request = serde_json::json!({
        "timestamp": timestamp,
        "signature": generate_vpc_signature(timestamp, "test_vpc_secret_123"),
        "client_id": format!("revoked-{}", uuid::Uuid::new_v4())
    });
    let response = server.post("/v1/auth/vpc/login").json(&request).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<VpcLoginResponse>();
    // Transfer ownership before revoking membership, leaving a valid organization.
    let (new_owner_session, _) = setup_unique_test_session(&database).await;
    let new_owner = uuid::Uuid::parse_str(new_owner_session.strip_prefix("rt_").unwrap()).unwrap();
    let client = database.pool().get().await.unwrap();
    client.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", &[&body.organization.id.0, &new_owner]).await.unwrap();
    client
        .execute(
            "DELETE FROM organization_members WHERE user_id = $1 AND organization_id = $2",
            &[&body.session.user_id.0, &body.organization.id.0],
        )
        .await
        .unwrap();
    let response = server.post("/v1/auth/vpc/login").json(&request).await;
    assert_eq!(response.status_code(), 403, "{}", response.text());
}
