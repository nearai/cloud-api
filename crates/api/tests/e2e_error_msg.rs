mod common;

use common::*;

// ============================================
// Error Message E2E Tests
// ============================================

#[tokio::test]
async fn test_models_missing_auth_header_message() {
    let server = setup_test_server().await;

    let response = server.get("/v1/models").await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "missing_auth_header");
    assert_eq!(err.error.message, "Missing authorization header");
}

#[tokio::test]
async fn test_models_invalid_auth_header_format_message() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", "Token abc")
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_auth_header");
    assert_eq!(err.error.message, "Invalid authorization header format");
}

#[tokio::test]
async fn test_models_invalid_api_key_message() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", "Bearer invalid_key_123")
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "invalid_api_key");
    assert_eq!(err.error.message, "Invalid or expired API key");
}

// ============================================
// Organizations Error Messages
// ============================================

#[tokio::test]
async fn test_organizations_duplicate_name_conflict_message() {
    let server = setup_test_server().await;

    let org_name = format!("test-org-{}", uuid::Uuid::new_v4());

    let req = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("desc".to_string()),
        display_name: Some("dn".to_string()),
    };

    let resp1 = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;
    assert_eq!(resp1.status_code(), 200);

    let resp2 = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;

    assert_eq!(resp2.status_code(), 409);
    let err = resp2.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "conflict");
    assert_eq!(err.error.message, "Organization already exists");
}

#[tokio::test]
async fn test_organizations_not_found() {
    let server = setup_test_server().await;

    // Random org id current user is not a member of
    let random_org_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/organizations/{}", random_org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Organization not found");
}

// ============================================
// Workspaces Error Messages
// ============================================

#[tokio::test]
async fn test_workspaces_duplicate_name_conflict_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let ws_name = format!("ws-{}", uuid::Uuid::new_v4());

    // Create first workspace
    let req = api::routes::workspaces::CreateWorkspaceRequest {
        name: ws_name.clone(),
        display_name: Some("First".to_string()),
        description: Some("desc".to_string()),
    };

    let resp1 = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;
    assert_eq!(resp1.status_code(), 201);

    // Duplicate
    let resp2 = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;

    assert_eq!(resp2.status_code(), 409);
    let err = resp2.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "conflict");
    assert_eq!(
        err.error.message,
        "Workspace name already exists in organization"
    );
}

#[tokio::test]
async fn test_users_me_missing_authorization_message() {
    let server = setup_test_server().await;

    let response = server.get("/v1/users/me").await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
}

#[tokio::test]
async fn test_users_me_wrong_bearer_prefix_message() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", "Token xyz")
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(
        err.error.message,
        "Authorization header does not start with 'Bearer '"
    );
}
