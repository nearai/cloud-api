mod common;

use common::*;

// ============================================
// Users error message tests
// ============================================

#[tokio::test]
async fn test_users_refresh_tokens_missing_authorization_message() {
    let server = setup_test_server().await;

    let response = server.get("/v1/users/me/refresh-tokens").await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
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

// ============================================
// Organization error message tests
// ============================================

#[tokio::test]
async fn test_list_organizations_missing_authorization_message() {
    let server = setup_test_server().await;

    let response = server.get("/v1/organizations").await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
}

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

    let random_org_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/organizations/{random_org_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Organization not found");
}

// ============================================
// Organization member error message tests
// ============================================

#[tokio::test]
async fn test_org_members_list_missing_authorization_message() {
    let server = setup_test_server().await;
    let random_org_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/organizations/{random_org_id}/members").as_str())
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
}

#[tokio::test]
async fn test_org_members_list_not_found_message() {
    let server = setup_test_server().await;
    let random_org_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/organizations/{random_org_id}/members").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Organization not found");
}

#[tokio::test]
async fn test_org_members_add_bad_request_invalid_user_id_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let response = server
        .post(format!("/v1/organizations/{}/members", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "user_id": "not-a-uuid",
            "role": "member"
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "bad_request");
    assert_eq!(err.error.message, "Invalid user ID");
}

#[tokio::test]
async fn test_org_members_remove_not_found_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let response = server
        .delete(
            format!(
                "/v1/organizations/{}/members/{}",
                org.id,
                uuid::Uuid::new_v4()
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Organization member not found");
}

// ============================================
// Workspaces error message tests
// ============================================

#[tokio::test]
async fn test_workspaces_duplicate_name_conflict_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let ws_name = format!("ws-{}", uuid::Uuid::new_v4());

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
async fn test_workspace_get_not_found_message() {
    let server = setup_test_server().await;

    let random_ws_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/workspaces/{random_ws_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Workspace not found");
}

#[tokio::test]
async fn test_list_workspace_api_keys_not_found_message() {
    let server = setup_test_server().await;

    let random_ws_id = uuid::Uuid::new_v4();

    let response = server
        .get(format!("/v1/workspaces/{random_ws_id}/api-keys").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 404);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "not_found");
    assert_eq!(err.error.message, "Workspace not found");
}

#[tokio::test]
async fn test_create_workspace_missing_authorization_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let req = api::routes::workspaces::CreateWorkspaceRequest {
        name: format!("ws-noauth-{}", uuid::Uuid::new_v4()),
        display_name: Some("No Auth".to_string()),
        description: Some("desc".to_string()),
    };

    let response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .json(&req)
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
}

#[tokio::test]
async fn test_list_workspace_api_keys_missing_authorization_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .await;

    assert_eq!(response.status_code(), 401);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "unauthorized");
    assert_eq!(err.error.message, "Missing authorization");
}

#[tokio::test]
async fn test_create_api_key_duplicate_name_conflict_message() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    let req = api::models::CreateApiKeyRequest {
        name: "dup-key".to_string(),
        expires_at: None,
        spend_limit: None,
    };

    let first = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;
    assert_eq!(first.status_code(), 201);

    let dup = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&req)
        .await;

    assert_eq!(dup.status_code(), 409);
    let err = dup.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.r#type, "duplicate_api_key_name");
    assert_eq!(
        err.error.message,
        "API key with this name already exists in this workspace"
    );
}

// ============================================
// Models error message tests
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
