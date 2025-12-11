mod common;

use common::*;

// ============================================
// Refresh Token and Access Token Tests
// ============================================

#[tokio::test]
async fn test_create_access_token_from_refresh_token() {
    let server = setup_test_server(None).await;
    let refresh_token = get_session_id();

    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let token_response = response.json::<api::models::AccessAndRefreshTokenResponse>();
    assert!(
        !token_response.access_token.is_empty(),
        "Access token should not be empty"
    );

    println!("✅ Successfully created access token from refresh token");
}

#[tokio::test]
async fn test_create_access_token_without_auth() {
    let server = setup_test_server(None).await;

    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should reject request without authorization"
    );

    println!("✅ Correctly rejected request without authorization");
}

#[tokio::test]
async fn test_create_access_token_with_invalid_refresh_token() {
    let server = setup_test_server(None).await;

    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", "Bearer invalid_token_xyz")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should reject invalid refresh token"
    );

    println!("✅ Correctly rejected invalid refresh token");
}

#[tokio::test]
#[ignore] // MockAuthService doesn't distinguish between token types
async fn test_access_token_cannot_create_new_access_token() {
    let server = setup_test_server(None).await;

    // First get an access token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Try to use access token to create another access token (should fail in production)
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Access token should not work on refresh token endpoint"
    );

    println!("✅ Correctly rejected access token on refresh token endpoint");
}

#[tokio::test]
#[ignore] // MockAuthService doesn't distinguish between token types
async fn test_refresh_token_cannot_access_regular_endpoints() {
    let server = setup_test_server(None).await;
    let refresh_token = get_session_id();

    // Try to use refresh token on a regular endpoint (should fail in production)
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Refresh token should not work on regular endpoints"
    );

    println!("✅ Correctly rejected refresh token on regular endpoint");
}

#[tokio::test]
async fn test_list_user_refresh_tokens() {
    let server = setup_test_server(None).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let response = server
        .get("/v1/users/me/refresh-tokens")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);

    let tokens = response.json::<Vec<api::models::RefreshTokenResponse>>();
    // Note: In mock mode, this list may be empty since MockAuthService doesn't store refresh tokens in DB
    // In production with real database, this would contain actual refresh token records

    if !tokens.is_empty() {
        // Verify token structure if any exist
        let token = &tokens[0];
        assert!(!token.id.is_empty(), "Token should have an ID");
        assert!(!token.user_id.is_empty(), "Token should have a user ID");
        assert!(
            token.expires_at > token.created_at,
            "Token should have valid expiration"
        );
        println!("✅ Successfully listed {} refresh token(s)", tokens.len());
    } else {
        println!("✅ List refresh tokens endpoint works (empty in mock mode)");
    }
}

#[tokio::test]
async fn test_list_refresh_tokens_without_auth() {
    let server = setup_test_server(None).await;

    let response = server.get("/v1/users/me/refresh-tokens").await;

    assert_eq!(response.status_code(), 401, "Should require authentication");

    println!("✅ Correctly rejected request without authentication");
}

#[tokio::test]
async fn test_revoke_all_tokens() {
    let server = setup_test_server(None).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let response = server
        .delete("/v1/users/me/tokens")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);

    let result = response.json::<serde_json::Value>();
    assert!(
        result["message"].as_str().unwrap().contains("Revoked"),
        "Should confirm revocation"
    );
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("access tokens"),
        "Should mention access tokens were invalidated"
    );
    assert!(
        result["count"].is_number(),
        "Should return count of revoked tokens"
    );

    println!("✅ Successfully revoked all tokens");
}

#[tokio::test]
#[ignore] // MockAuthService doesn't track tokens_revoked_at
async fn test_revoke_all_tokens_invalidates_access_token() {
    let server = setup_test_server(None).await;
    let refresh_token = get_session_id();

    // Create an access token
    let access_token = get_access_token_from_refresh_token(&server, refresh_token.clone()).await;

    // Verify access token works
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;
    assert_eq!(response.status_code(), 200, "Access token should work");

    // Revoke all tokens
    let revoke_response = server
        .delete("/v1/users/me/tokens")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;
    assert_eq!(revoke_response.status_code(), 200);

    // Try to use the old access token (should fail in production)
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Old access token should be invalidated"
    );

    println!("✅ Access token correctly invalidated after revoking all tokens");
}

#[tokio::test]
#[ignore] // MockAuthService doesn't track deleted refresh tokens
async fn test_revoke_all_tokens_prevents_refresh_token_use() {
    let server = setup_test_server(None).await;
    let refresh_token = get_session_id();

    // Create an access token first
    let access_token = get_access_token_from_refresh_token(&server, refresh_token.clone()).await;

    // Revoke all tokens using the access token
    let revoke_response = server
        .delete("/v1/users/me/tokens")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;
    assert_eq!(revoke_response.status_code(), 200);

    // Try to create a new access token with the old refresh token (should fail in production)
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Old refresh token should be deleted"
    );

    println!("✅ Refresh token correctly deleted after revoking all tokens");
}

#[tokio::test]
async fn test_revoke_all_tokens_without_auth() {
    let server = setup_test_server(None).await;

    let response = server.delete("/v1/users/me/tokens").await;

    assert_eq!(response.status_code(), 401, "Should require authentication");

    println!("✅ Correctly rejected request without authentication");
}

#[tokio::test]
async fn test_access_token_works_on_regular_endpoints() {
    let server = setup_test_server(None).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Test on user endpoint
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200, "Access token should work");

    let user = response.json::<api::models::UserResponse>();
    assert!(!user.id.is_empty(), "Should return user data");
    assert!(!user.email.is_empty(), "Should return user email");

    println!("✅ Access token works correctly on regular endpoints");
}

#[tokio::test]
async fn test_access_token_can_create_organization() {
    let server = setup_test_server(None).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let create_request = serde_json::json!({
        "name": format!("test-org-{}", uuid::Uuid::new_v4()),
        "display_name": "Test Organization",
        "description": "Created with access token"
    });

    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&create_request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should be able to create organization with access token"
    );

    let org = response.json::<api::models::OrganizationResponse>();
    assert!(!org.id.is_empty(), "Organization should have an ID");
    assert_eq!(org.name, create_request["name"].as_str().unwrap());

    println!("✅ Access token can perform authenticated actions");
}

#[tokio::test]
async fn test_create_access_token_success_user_agent_match() {
    // Spin up full test server (with MockAuthService, DB, etc.)
    let server = setup_test_server(None).await;

    // Simulate refresh token for user
    let refresh_token = "rt_mock_refresh_token";
    let user_agent = MOCK_USER_AGENT;

    // Perform API request
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", user_agent)
        .await;

    // Validate response
    assert_eq!(response.status_code(), 200, "Expected 200 OK");
    let body = response.json::<api::models::AccessAndRefreshTokenResponse>();

    assert!(
        body.access_token.len() > 10,
        "Access token should be non-empty"
    );
    assert!(
        body.refresh_token.starts_with("rt_"),
        "Refresh token should start with rt_"
    );

    println!("✅ Success: {body:?}");
}

#[tokio::test]
async fn test_create_access_token_success_user_agent_mismatch() {
    let server = setup_test_server(None).await;

    let refresh_token = "rt_mock_refresh_token";
    let user_agent = "Different User Agent";

    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", user_agent)
        .await;

    assert_eq!(response.status_code(), 401, "Expected 401 Unauthorized");
    println!("Correctly rejected mismatched User-Agent");
}

#[tokio::test]
#[ignore] // Requires real database to test token rotation properly
async fn test_refresh_token_rotation_invalidates_old_token() {
    let server = setup_test_server(None).await;
    let old_refresh_token = get_session_id();

    // Create an access token using the old refresh token
    let response1 = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {old_refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response1.status_code(), 200, "First request should succeed");
    let token_response1 = response1.json::<api::models::AccessAndRefreshTokenResponse>();
    let new_refresh_token = token_response1.refresh_token.clone();
    assert!(
        !new_refresh_token.is_empty(),
        "Should receive new refresh token"
    );
    assert_ne!(
        old_refresh_token, new_refresh_token,
        "New refresh token should be different from old one"
    );

    // Validate that the new refresh token has an extended expiration time (7 days = 168 hours)
    let now = chrono::Utc::now();
    let expected_expiration = now + chrono::Duration::hours(7 * 24);
    let actual_expiration = token_response1.refresh_token_expiration;

    // Allow 5 minutes tolerance for test execution time
    let tolerance = chrono::Duration::minutes(5);
    let min_expected = expected_expiration - tolerance;
    let max_expected = expected_expiration + tolerance;

    assert!(
        actual_expiration >= min_expected && actual_expiration <= max_expected,
        "New refresh token expiration should be approximately 7 days from now. Expected: {expected_expiration:?}, Actual: {actual_expiration:?}"
    );

    assert!(
        actual_expiration > now,
        "Refresh token expiration should be in the future. Now: {now:?}, Expiration: {actual_expiration:?}"
    );

    // Try to use the old refresh token again (should fail - token rotation invalidated it)
    let response2 = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {old_refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response2.status_code(),
        401,
        "Old refresh token should be invalidated after rotation"
    );

    // Verify the new refresh token works
    let response3 = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {new_refresh_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response3.status_code(),
        200,
        "New refresh token should work after rotation"
    );

    let token_response3 = response3.json::<api::models::AccessAndRefreshTokenResponse>();
    assert!(
        !token_response3.access_token.is_empty(),
        "Should receive access token with new refresh token"
    );

    // Validate that the rotated refresh token also has extended expiration
    let now_after_rotation = chrono::Utc::now();
    let expected_expiration_after_rotation = now_after_rotation + chrono::Duration::hours(7 * 24);
    let actual_expiration_after_rotation = token_response3.refresh_token_expiration;

    let tolerance = chrono::Duration::minutes(5);
    let min_expected_after = expected_expiration_after_rotation - tolerance;
    let max_expected_after = expected_expiration_after_rotation + tolerance;

    assert!(
        actual_expiration_after_rotation >= min_expected_after
            && actual_expiration_after_rotation <= max_expected_after,
        "Rotated refresh token expiration should be approximately 7 days from now. Expected: {expected_expiration_after_rotation:?}, Actual: {actual_expiration_after_rotation:?}"
    );

    println!("✅ Refresh token rotation correctly invalidates old token and extends expiration");
}

// Note: Specific refresh token revocation (DELETE /users/me/refresh-tokens/{id})
// is difficult to test in the current mock setup because we don't have easy access
// to actual refresh token IDs. In a real database scenario, these would be testable.
