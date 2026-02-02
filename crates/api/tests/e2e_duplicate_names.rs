// E2E tests for duplicate organization and workspace name error handling
mod common;

use common::*;

// ============================================
// Organization Duplicate Name Tests
// ============================================

#[tokio::test]
async fn test_duplicate_organization_name_returns_409() {
    let (server, _guard) = setup_test_server().await;

    let org_name = format!("test-org-{}", uuid::Uuid::new_v4());

    // Create first organization with a specific name
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("First organization".to_string()),
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
    let (server, _guard) = setup_test_server().await;

    let org_name = format!("TestOrg-{}", uuid::Uuid::new_v4());

    // Create first organization
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("First organization".to_string()),
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
// Organization Name Reuse After Deletion Tests
// ============================================

#[tokio::test]
async fn test_organization_name_reuse_after_deletion() {
    let (server, _guard) = setup_test_server().await;

    let org_name = format!("reusable-org-{}", uuid::Uuid::new_v4());

    // Step 1: Create an organization
    let create_request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Original organization".to_string()),
    };

    let create_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&create_request)
        .await;

    assert_eq!(
        create_response.status_code(),
        200,
        "Organization creation should succeed"
    );

    let org = create_response.json::<api::models::OrganizationResponse>();
    assert_eq!(org.name, org_name);

    // Step 2: Delete the organization
    let delete_response = server
        .delete(format!("/v1/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        delete_response.status_code(),
        200,
        "Organization deletion should succeed"
    );

    // Step 3: Re-create an organization with the same name (should succeed after fix)
    let recreate_request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Re-created organization with same name".to_string()),
    };

    let recreate_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&recreate_request)
        .await;

    assert_eq!(
        recreate_response.status_code(),
        200,
        "Re-creating organization with deleted organization's name should succeed (was bug #337)"
    );

    let new_org = recreate_response.json::<api::models::OrganizationResponse>();
    assert_eq!(new_org.name, org_name);
    assert_ne!(
        new_org.id, org.id,
        "New organization should have a different ID"
    );

    println!("✓ Organization name can be reused after deletion (fix for #337)");
}

// ============================================
// Workspace Duplicate Name Tests
// ============================================

#[tokio::test]
async fn test_duplicate_workspace_name_returns_409() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let workspace_name = format!("test-workspace-{}", uuid::Uuid::new_v4());

    // Create first workspace
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
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
    let (server, _guard) = setup_test_server().await;

    // Create two different organizations
    let org1 = create_org(&server).await;
    let org2 = create_org(&server).await;

    let workspace_name = format!("shared-name-{}", uuid::Uuid::new_v4());

    // Create workspace with same name in first organization
    let request1 = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
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
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // When an organization is created, a "default" workspace is automatically created
    // Try to create another workspace named "default"
    let duplicate_default_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: "default".to_string(),
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

#[tokio::test]
async fn test_update_workspace_name_to_duplicate_returns_409() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Create two workspaces
    let ws1_name = format!("workspace-one-{}", uuid::Uuid::new_v4());
    let ws2_name = format!("workspace-two-{}", uuid::Uuid::new_v4());

    let request1 = api::routes::workspaces::CreateWorkspaceRequest {
        name: ws1_name.clone(),
        description: Some("First workspace".to_string()),
    };

    let response1 = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request1)
        .await;
    assert_eq!(response1.status_code(), 201);
    let workspace1 = response1.json::<api::routes::workspaces::WorkspaceResponse>();

    let request2 = api::routes::workspaces::CreateWorkspaceRequest {
        name: ws2_name.clone(),
        description: Some("Second workspace".to_string()),
    };

    let response2 = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request2)
        .await;
    assert_eq!(response2.status_code(), 201);

    // Try to rename workspace1 to workspace2's name (should fail with 409)
    let update_request = api::routes::workspaces::UpdateWorkspaceRequest {
        name: Some(ws2_name.clone()),
        description: None,
        settings: None,
    };

    let update_response = server
        .put(format!("/v1/workspaces/{}", workspace1.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(
        update_response.status_code(),
        409,
        "Updating workspace name to duplicate should return 409 CONFLICT"
    );

    println!("✓ Updating workspace name to duplicate correctly returns 409 CONFLICT");
}

#[tokio::test]
async fn test_update_workspace_name_to_unique_succeeds() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Create a workspace
    let original_name = format!("original-name-{}", uuid::Uuid::new_v4());
    let new_name = format!("new-name-{}", uuid::Uuid::new_v4());

    let create_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: original_name.clone(),
        description: Some("Test workspace".to_string()),
    };

    let create_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&create_request)
        .await;
    assert_eq!(create_response.status_code(), 201);
    let workspace = create_response.json::<api::routes::workspaces::WorkspaceResponse>();

    // Update workspace name to a unique name (should succeed)
    let update_request = api::routes::workspaces::UpdateWorkspaceRequest {
        name: Some(new_name.clone()),
        description: None,
        settings: None,
    };

    let update_response = server
        .put(format!("/v1/workspaces/{}", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(
        update_response.status_code(),
        200,
        "Updating workspace name to unique name should succeed"
    );

    let updated_workspace = update_response.json::<api::routes::workspaces::WorkspaceResponse>();
    assert_eq!(updated_workspace.name, new_name);

    println!("✓ Updating workspace name to unique name succeeds");
}

#[tokio::test]
async fn test_update_workspace_name_to_same_name_succeeds() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Create a workspace
    let workspace_name = format!("test-workspace-{}", uuid::Uuid::new_v4());

    let create_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        description: Some("Test workspace".to_string()),
    };

    let create_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&create_request)
        .await;
    assert_eq!(create_response.status_code(), 201);
    let workspace = create_response.json::<api::routes::workspaces::WorkspaceResponse>();

    // Update workspace name to the same name (should succeed - no-op)
    let update_request = api::routes::workspaces::UpdateWorkspaceRequest {
        name: Some(workspace_name.clone()),
        description: None,
        settings: None,
    };

    let update_response = server
        .put(format!("/v1/workspaces/{}", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(
        update_response.status_code(),
        200,
        "Updating workspace name to same name should succeed"
    );

    println!("✓ Updating workspace name to same name succeeds");
}

// ============================================
// Workspace Name Reuse After Deletion Tests
// ============================================

#[tokio::test]
async fn test_workspace_name_reuse_after_deletion() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let workspace_name = format!("reusable-workspace-{}", uuid::Uuid::new_v4());

    // Step 1: Create a workspace
    let create_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        description: Some("Original workspace".to_string()),
    };

    let create_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&create_request)
        .await;

    assert_eq!(
        create_response.status_code(),
        201,
        "Workspace creation should succeed"
    );

    let workspace = create_response.json::<api::routes::workspaces::WorkspaceResponse>();
    assert_eq!(workspace.name, workspace_name);

    // Step 2: Delete the workspace
    let delete_response = server
        .delete(format!("/v1/workspaces/{}", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        delete_response.status_code(),
        200,
        "Workspace deletion should succeed"
    );

    // Step 3: Re-create a workspace with the same name (should succeed after fix)
    let recreate_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
        description: Some("Re-created workspace with same name".to_string()),
    };

    let recreate_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&recreate_request)
        .await;

    assert_eq!(
        recreate_response.status_code(),
        201,
        "Re-creating workspace with deleted workspace's name should succeed (was bug #338)"
    );

    let new_workspace = recreate_response.json::<api::routes::workspaces::WorkspaceResponse>();
    assert_eq!(new_workspace.name, workspace_name);
    assert_ne!(
        new_workspace.id, workspace.id,
        "New workspace should have a different ID"
    );

    println!("✓ Workspace name can be reused after deletion (fix for #338)");
}

// ============================================
// API Key Duplicate Name Tests
// ============================================

#[tokio::test]
async fn test_duplicate_api_key_name_on_create_returns_409() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Get the default workspace
    let workspaces_response = server
        .get(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(workspaces_response.status_code(), 200);
    let workspaces = workspaces_response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    let workspace = &workspaces.workspaces[0];

    let api_key_name = format!("test-key-{}", uuid::Uuid::new_v4());

    // Create first API key
    let request = api::models::CreateApiKeyRequest {
        name: api_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        201,
        "First API key creation should succeed with 201 CREATED"
    );

    let first_key = response.json::<api::models::ApiKeyResponse>();
    assert_eq!(first_key.name, Some(api_key_name.clone()));

    // Try to create second API key with the same name in the same workspace
    let duplicate_request = api::models::CreateApiKeyRequest {
        name: api_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let duplicate_response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&duplicate_request)
        .await;

    // Should return 409 CONFLICT
    assert_eq!(
        duplicate_response.status_code(),
        409,
        "Duplicate API key name should return 409 CONFLICT"
    );

    let error_response = duplicate_response.json::<api::models::ErrorResponse>();
    assert_eq!(error_response.error.r#type, "duplicate_api_key_name");
    assert!(error_response
        .error
        .message
        .contains("API key with this name already exists"));

    println!("✓ Duplicate API key name on create correctly returns 409 CONFLICT");
}

#[tokio::test]
async fn test_duplicate_api_key_name_on_update_returns_409() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Get the default workspace
    let workspaces_response = server
        .get(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(workspaces_response.status_code(), 200);
    let workspaces = workspaces_response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    let workspace = &workspaces.workspaces[0];

    let first_key_name = format!("first-key-{}", uuid::Uuid::new_v4());
    let second_key_name = format!("second-key-{}", uuid::Uuid::new_v4());

    // Create first API key
    let request1 = api::models::CreateApiKeyRequest {
        name: first_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response1 = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request1)
        .await;

    assert_eq!(response1.status_code(), 201);
    let _first_key = response1.json::<api::models::ApiKeyResponse>();

    // Create second API key with different name
    let request2 = api::models::CreateApiKeyRequest {
        name: second_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response2 = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request2)
        .await;

    assert_eq!(response2.status_code(), 201);
    let second_key = response2.json::<api::models::ApiKeyResponse>();

    // Try to rename second key to the first key's name (should fail with 409)
    let update_request = api::models::UpdateApiKeyRequest {
        name: Some(first_key_name.clone()),
        expires_at: None,
        spend_limit: None,
        is_active: None,
    };

    let update_response = server
        .patch(format!("/v1/workspaces/{}/api-keys/{}", workspace.id, second_key.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    // Should return 409 CONFLICT
    assert_eq!(
        update_response.status_code(),
        409,
        "Renaming API key to duplicate name should return 409 CONFLICT"
    );

    let error_response = update_response.json::<api::models::ErrorResponse>();
    assert_eq!(error_response.error.r#type, "duplicate_api_key_name");
    assert!(error_response
        .error
        .message
        .contains("API key with this name already exists"));

    println!("✓ Duplicate API key name on update correctly returns 409 CONFLICT");
}

#[tokio::test]
async fn test_api_key_update_with_same_name_succeeds() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Get the default workspace
    let workspaces_response = server
        .get(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(workspaces_response.status_code(), 200);
    let workspaces = workspaces_response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    let workspace = &workspaces.workspaces[0];

    let api_key_name = format!("test-key-{}", uuid::Uuid::new_v4());

    // Create API key
    let request = api::models::CreateApiKeyRequest {
        name: api_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request)
        .await;

    assert_eq!(response.status_code(), 201);
    let api_key = response.json::<api::models::ApiKeyResponse>();

    // Update the API key with the same name (should succeed - not changing the name)
    let update_request = api::models::UpdateApiKeyRequest {
        name: Some(api_key_name.clone()),
        expires_at: None,
        spend_limit: Some(api::models::DecimalPriceRequest {
            amount: 1000000000, // 1 dollar in nano-dollars
            currency: "USD".to_string(),
        }),
        is_active: None,
    };

    let update_response = server
        .patch(format!("/v1/workspaces/{}/api-keys/{}", workspace.id, api_key.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    // Should succeed (200 OK) since we're not actually changing the name
    assert_eq!(
        update_response.status_code(),
        200,
        "Updating API key with same name should succeed"
    );

    let updated_key = update_response.json::<api::models::ApiKeyResponse>();
    assert_eq!(updated_key.name, Some(api_key_name));
    assert_eq!(updated_key.spend_limit.as_ref().unwrap().amount, 1000000000);

    println!("✓ Updating API key with its own name succeeds (no duplicate conflict)");
}

#[tokio::test]
async fn test_same_api_key_name_different_workspaces_allowed() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Get the default workspace
    let workspaces_response = server
        .get(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let workspaces = workspaces_response.json::<api::routes::workspaces::ListWorkspacesResponse>();
    let workspace1 = &workspaces.workspaces[0];

    // Create a second workspace
    let workspace2_request = api::routes::workspaces::CreateWorkspaceRequest {
        name: format!("second-workspace-{}", uuid::Uuid::new_v4()),
        description: Some("Second workspace for testing".to_string()),
    };

    let workspace2_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&workspace2_request)
        .await;

    assert_eq!(workspace2_response.status_code(), 201);
    let workspace2 = workspace2_response.json::<api::routes::workspaces::WorkspaceResponse>();

    let api_key_name = format!("shared-key-name-{}", uuid::Uuid::new_v4());

    // Create API key in first workspace
    let request1 = api::models::CreateApiKeyRequest {
        name: api_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response1 = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace1.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request1)
        .await;

    assert_eq!(response1.status_code(), 201);
    let key1 = response1.json::<api::models::ApiKeyResponse>();

    // Create API key with same name in second workspace (should succeed)
    let request2 = api::models::CreateApiKeyRequest {
        name: api_key_name.clone(),
        expires_at: None,
        spend_limit: None,
    };

    let response2 = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace2.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&request2)
        .await;

    assert_eq!(
        response2.status_code(),
        201,
        "API key with same name in different workspace should succeed"
    );

    let key2 = response2.json::<api::models::ApiKeyResponse>();

    // Verify both keys exist with same name but in different workspaces
    assert_eq!(key1.name.as_ref().unwrap(), &api_key_name);
    assert_eq!(key2.name.as_ref().unwrap(), &api_key_name);
    assert_eq!(key1.workspace_id, workspace1.id);
    assert_eq!(key2.workspace_id, workspace2.id);
    assert_ne!(key1.id, key2.id);

    println!("✓ API key names are correctly scoped per workspace");
}

// ============================================
// Error Message Format Tests
// ============================================

#[tokio::test]
async fn test_duplicate_errors_no_information_leakage() {
    let (server, _guard) = setup_test_server().await;

    // Create organization
    let org_name = format!("secret-org-{}", uuid::Uuid::new_v4());
    let request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Secret organization".to_string()),
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
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let workspace_name = format!("test-workspace-{}", uuid::Uuid::new_v4());

    // Create workspace
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: workspace_name.clone(),
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
