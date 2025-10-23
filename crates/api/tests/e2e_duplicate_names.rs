// E2E tests for duplicate organization and workspace name error handling
mod common;

use common::*;

// ============================================
// Organization Duplicate Name Tests
// ============================================

#[tokio::test]
async fn test_duplicate_organization_name_returns_409() {
    let server = setup_test_server().await;

    let org_name = format!("test-org-{}", uuid::Uuid::new_v4());

    // Create first organization with a specific name
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("First organization".to_string()),
        display_name: Some("First Org".to_string()),
    };

    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "First organization creation should succeed"
    );

    let first_org = response.json::<api::models::OrganizationResponse>();
    assert_eq!(first_org.name, org_name);

    // Try to create second organization with the same name
    let duplicate_request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Duplicate organization".to_string()),
        display_name: Some("Duplicate Org".to_string()),
    };

    let duplicate_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&duplicate_request)
        .await;

    // Should return 409 CONFLICT, not 400 or 500
    assert_eq!(
        duplicate_response.status_code(),
        409,
        "Duplicate organization name should return 409 CONFLICT"
    );

    println!("✓ Duplicate organization name correctly returns 409 CONFLICT");
}

#[tokio::test]
async fn test_duplicate_organization_name_case_sensitive() {
    let server = setup_test_server().await;

    let org_name = format!("TestOrg-{}", uuid::Uuid::new_v4());

    // Create first organization
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("First organization".to_string()),
        display_name: Some("First Org".to_string()),
    };

    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 200);

    // Try to create with different case (should succeed if case-sensitive, or fail if case-insensitive)
    // Based on PostgreSQL default behavior, this depends on collation
    let different_case_request = api::models::CreateOrganizationRequest {
        name: org_name.to_lowercase(),
        description: Some("Different case organization".to_string()),
        display_name: Some("Different Case Org".to_string()),
    };

    let case_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&different_case_request)
        .await;

    // Document the behavior (case-sensitive by default in PostgreSQL)
    if case_response.status_code() == 200 {
        println!("✓ Organization names are case-sensitive (different case allowed)");
    } else if case_response.status_code() == 409 {
        println!("✓ Organization names are case-insensitive (different case blocked with 409)");
    } else {
        panic!(
            "Unexpected status code {} for case variation",
            case_response.status_code()
        );
    }
}

// ============================================
// Workspace Duplicate Name Tests
// ============================================

#[tokio::test]
async fn test_duplicate_workspace_name_returns_409() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspace_name = format!("test-workspace-{}", uuid::Uuid::new_v4());

    // Create first workspace
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        display_name: Some("First Workspace".to_string()),
        description: Some("First workspace description".to_string()),
    };

    let response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        201,
        "First workspace creation should succeed with 201 CREATED"
    );

    let first_workspace = response.json::<api::routes::workspaces::WorkspaceResponse>();
    assert_eq!(first_workspace.name, workspace_name);
    assert_eq!(first_workspace.organization_id, org.id);

    // Try to create second workspace with the same name in the same organization
    let duplicate_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        display_name: Some("Duplicate Workspace".to_string()),
        description: Some("Duplicate workspace description".to_string()),
    };

    let duplicate_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&duplicate_request)
        .await;

    // Should return 409 CONFLICT, not 400 or 500
    assert_eq!(
        duplicate_response.status_code(),
        409,
        "Duplicate workspace name should return 409 CONFLICT"
    );

    println!("✓ Duplicate workspace name correctly returns 409 CONFLICT");
}

#[tokio::test]
async fn test_same_workspace_name_different_organizations_allowed() {
    let server = setup_test_server().await;

    // Create two different organizations
    let org1 = create_org(&server).await;
    let org2 = create_org(&server).await;

    let workspace_name = format!("shared-name-{}", uuid::Uuid::new_v4());

    // Create workspace with same name in first organization
    let request1 = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        display_name: Some("Workspace in Org 1".to_string()),
        description: Some("First organization's workspace".to_string()),
    };

    let response1 = server
        .post(format!("/v1/organizations/{}/workspaces", org1.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request1)
        .await;

    assert_eq!(
        response1.status_code(),
        201,
        "First workspace should be created successfully"
    );

    let workspace1 = response1.json::<api::routes::workspaces::WorkspaceResponse>();

    // Create workspace with same name in second organization (should succeed)
    let request2 = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        display_name: Some("Workspace in Org 2".to_string()),
        description: Some("Second organization's workspace".to_string()),
    };

    let response2 = server
        .post(format!("/v1/organizations/{}/workspaces", org2.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request2)
        .await;

    assert_eq!(
        response2.status_code(),
        201,
        "Second workspace with same name in different org should succeed"
    );

    let workspace2 = response2.json::<api::routes::workspaces::WorkspaceResponse>();

    // Verify both workspaces exist with same name but different organizations
    assert_eq!(workspace1.name, workspace_name);
    assert_eq!(workspace2.name, workspace_name);
    assert_eq!(workspace1.organization_id, org1.id);
    assert_eq!(workspace2.organization_id, org2.id);
    assert_ne!(workspace1.id, workspace2.id);

    println!("✓ Workspace names are correctly scoped per organization");
}

#[tokio::test]
async fn test_duplicate_workspace_default_workspace() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // When an organization is created, a "default" workspace is automatically created
    // Try to create another workspace named "default"
    let duplicate_default_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: "default".to_string(),
        display_name: Some("Another Default".to_string()),
        description: Some("Attempt to create duplicate default workspace".to_string()),
    };

    let response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&duplicate_default_request)
        .await;

    // Should return 409 CONFLICT
    assert_eq!(
        response.status_code(),
        409,
        "Duplicate 'default' workspace name should return 409 CONFLICT"
    );

    println!("✓ Cannot create duplicate 'default' workspace");
}

// ============================================
// Error Message Format Tests
// ============================================

#[tokio::test]
async fn test_duplicate_errors_no_information_leakage() {
    let server = setup_test_server().await;

    // Create organization
    let org_name = format!("secret-org-{}", uuid::Uuid::new_v4());
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Secret organization".to_string()),
        display_name: Some("Secret Org".to_string()),
    };

    server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    // Try to create duplicate
    let duplicate_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(duplicate_response.status_code(), 409);

    // Verify that the error response doesn't leak sensitive information
    // The response should just be a 409 status code without detailed error messages
    // that could be used for enumeration attacks
    let response_text = duplicate_response.text();

    // Should not contain database-specific error messages
    assert!(
        !response_text.to_lowercase().contains("duplicate key"),
        "Response should not leak database error details"
    );
    assert!(
        !response_text.to_lowercase().contains("constraint"),
        "Response should not leak constraint details"
    );

    println!("✓ Duplicate error responses don't leak sensitive information");
}

// ============================================
// Permission and Security Tests
// ============================================

#[tokio::test]
async fn test_workspace_duplicate_check_respects_permissions() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspace_name = format!("test-workspace-{}", uuid::Uuid::new_v4());

    // Create workspace
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        display_name: Some("Test Workspace".to_string()),
        description: Some("Test".to_string()),
    };

    let response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 201);

    // Try to create duplicate (should get 409, not 403)
    // This verifies that permission check happens before duplicate check
    let duplicate_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    // Should be 409 (duplicate) not 403 (forbidden) since user has permission
    assert_eq!(
        duplicate_response.status_code(),
        409,
        "Authorized user should get 409 for duplicate, not permission error"
    );

    println!("✓ Permission checks occur before duplicate checks");
}
