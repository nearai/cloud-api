// E2E tests for duplicate organization labels and scoped workspace names

use crate::common::*;

// Organization names are labels. UUIDs identify tenants even when names match.
async fn create_named_organization(
    server: &axum_test::TestServer,
    session: &str,
    name: &str,
) -> api::models::OrganizationResponse {
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {session}"))
        .json(&serde_json::json!({ "name": name }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let org = response.json::<api::models::OrganizationResponse>();
    assert_eq!(org.name, name);
    org
}

#[tokio::test]
async fn test_near_personal_organization_with_non_unique_names() {
    let (server, database) = setup_test_server_with_database().await;
    let account = format!("duplicate-review-{}.testnet", uuid::Uuid::new_v4().simple());
    let users = database::repositories::UserRepository::new(database.pool().clone());
    // This path invokes the persisted provisioning function. Removing the name
    // index without replacing its ON CONFLICT(name) clause breaks first login.
    let user = users
        .create_from_oauth(
            format!("{account}@near"),
            account.clone(),
            None,
            None,
            "near".to_string(),
            account,
        )
        .await
        .expect("NEAR first login must still provision a personal organization");
    let client = database.pool().get().await.unwrap();
    let personal_id: uuid::Uuid = client
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id = $1 AND role = 'owner'",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    let shared_name = format!("near-personal-{}", uuid::Uuid::new_v4());
    let other = create_named_organization(&server, &get_session_id(), &shared_name).await;
    client
        .execute(
            "UPDATE organizations SET name = $1 WHERE id = $2",
            &[&shared_name, &personal_id],
        )
        .await
        .unwrap();
    let repeated_id: uuid::Uuid = client
        .query_one("SELECT ensure_near_personal_organization($1)", &[&user.id])
        .await
        .unwrap()
        .get(0);
    assert_eq!(repeated_id, personal_id);
    assert_ne!(personal_id.to_string(), other.id);
    let workspace_count: i64 = client
        .query_one(
            "SELECT count(*) FROM workspaces WHERE organization_id = $1 AND is_active = true",
            &[&personal_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(workspace_count, 1);
}

#[tokio::test]
async fn test_duplicate_organization_names_for_same_owner() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, _) = setup_unique_test_session(&database).await;
    let name = format!("same-owner-{}", uuid::Uuid::new_v4());

    // Concurrent creation must also produce independent organizations.
    let (first, second) = tokio::join!(
        create_named_organization(&server, &session, &name),
        create_named_organization(&server, &session, &name),
    );
    assert_ne!(first.id, second.id);
    assert_eq!(first.owner_id, second.owner_id);

    let response = server
        .get("/v1/organizations")
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let organizations = response
        .json::<api::models::ListOrganizationsResponse>()
        .organizations;
    assert_eq!(organizations.len(), 2);
    assert!(organizations.iter().any(|org| org.id == first.id));
    assert!(organizations.iter().any(|org| org.id == second.id));
}

#[tokio::test]
async fn test_duplicate_organization_names_for_different_owners_and_rename() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_a, _) = setup_unique_test_session(&database).await;
    let (session_b, _) = setup_unique_test_session(&database).await;
    let name = format!("different-owners-{}", uuid::Uuid::new_v4());
    let first = create_named_organization(&server, &session_a, &name).await;
    let second = create_named_organization(&server, &session_b, &name).await;
    assert_ne!(first.id, second.id);
    assert_ne!(first.owner_id, second.owner_id);

    let renamed = create_named_organization(&server, &session_b, &format!("{name}-other")).await;
    let response = server
        .put(format!("/v1/organizations/{}", renamed.id).as_str())
        .add_header("Authorization", format!("Bearer {session_b}"))
        .json(&serde_json::json!({ "name": name }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let updated = response.json::<api::models::OrganizationResponse>();
    assert_eq!(updated.id, renamed.id);
    assert_eq!(updated.name, name);

    // Matching the name does not grant access to the other owner's tenant.
    for (session, org_id) in [(&session_a, &second.id), (&session_b, &first.id)] {
        for suffix in ["", "/members", "/usage/balance", "/workspaces"] {
            let response = server
                .get(format!("/v1/organizations/{org_id}{suffix}").as_str())
                .add_header("Authorization", format!("Bearer {session}"))
                .await;
            assert_eq!(response.status_code(), 403, "{}", response.text());
        }
        let response = server
            .put(format!("/v1/organizations/{org_id}").as_str())
            .add_header("Authorization", format!("Bearer {session}"))
            .json(&serde_json::json!({ "name": "unauthorized rename" }))
            .await;
        assert_eq!(response.status_code(), 403, "{}", response.text());
        let response = server
            .delete(format!("/v1/organizations/{org_id}").as_str())
            .add_header("Authorization", format!("Bearer {session}"))
            .await;
        assert_eq!(response.status_code(), 403, "{}", response.text());
    }
}

#[tokio::test]
async fn test_duplicate_organization_resources_and_deletion_are_isolated() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner, _) = setup_unique_test_session(&database).await;
    let (member_session, _) = setup_unique_test_session(&database).await;
    // Keep every member's default organization out of this deletion test.
    let _default = create_org_with_session(&server, &owner).await;
    let _member_default = create_org_with_session(&server, &member_session).await;
    let name = format!("isolated-{}", uuid::Uuid::new_v4());
    let first = create_named_organization(&server, &owner, &name).await;
    let second = create_named_organization(&server, &owner, &format!("{name}-other")).await;
    let response = server
        .put(format!("/v1/organizations/{}", second.id).as_str())
        .add_header("Authorization", format!("Bearer {owner}"))
        .json(&serde_json::json!({ "name": name }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response.json::<api::models::OrganizationResponse>().id,
        second.id
    );

    let response = server
        .post(format!("/v1/organizations/{}/members", first.id).as_str())
        .add_header("Authorization", format!("Bearer {owner}"))
        .json(&serde_json::json!({
            "user_id": member_session.strip_prefix("rt_").unwrap(),
            "role": "member"
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let mut credentials = Vec::new();
    for (org, amount, expected_members) in [
        (&first, 10_000_000_000_i64, 2),
        (&second, 20_000_000_000, 1),
    ] {
        add_credits_with_type(
            &server,
            &org.id,
            "payment",
            None,
            amount,
            "USD",
            &get_session_id(),
        )
        .await;
        let response = server
            .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
            .add_header("Authorization", format!("Bearer {owner}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let balance = response.json::<api::routes::usage::OrganizationBalanceResponse>();
        assert_eq!(balance.organization_id, org.id);
        assert_eq!(balance.remaining, Some(amount));

        let response = server
            .get(format!("/v1/organizations/{}/members", org.id).as_str())
            .add_header("Authorization", format!("Bearer {owner}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let members = response.json::<serde_json::Value>();
        assert_eq!(members["total"], expected_members);
        assert!(members["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["organization_id"] == org.id));

        let workspaces = list_workspaces_with_session(&server, org.id.clone(), &owner).await;
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "default");
        assert_eq!(workspaces[0].organization_id, org.id);
        let key = create_api_key_in_workspace_with_session(
            &server,
            workspaces[0].id.clone(),
            "same-key-name".to_string(),
            &owner,
        )
        .await;
        let response = server
            .get(format!("/v1/workspaces/{}/api-keys", workspaces[0].id).as_str())
            .add_header("Authorization", format!("Bearer {owner}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let keys = response.json::<api::models::ListApiKeysResponse>().api_keys;
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, key.id);
        credentials.push((workspaces[0].id.clone(), key.key.unwrap()));
    }
    assert_ne!(credentials[0].0, credentials[1].0);
    assert_ne!(credentials[0].1, credentials[1].1);

    // A user joined to one same-named organization does not join the other.
    for (org, expected_status) in [(&first, 200), (&second, 403)] {
        let response = server
            .get(format!("/v1/organizations/{}", org.id).as_str())
            .add_header("Authorization", format!("Bearer {member_session}"))
            .await;
        assert_eq!(
            response.status_code(),
            expected_status,
            "{}",
            response.text()
        );
    }

    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", credentials[1].0).as_str())
        .add_header("Authorization", format!("Bearer {member_session}"))
        .await;
    assert_eq!(response.status_code(), 403, "{}", response.text());

    // Joining both organizations with the same label preserves both memberships.
    let response = server
        .post(format!("/v1/organizations/{}/members", second.id).as_str())
        .add_header("Authorization", format!("Bearer {owner}"))
        .json(&serde_json::json!({
            "user_id": member_session.strip_prefix("rt_").unwrap(),
            "role": "member"
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {member_session}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let me = response.json::<serde_json::Value>();
    let memberships = me["organizations"].as_array().unwrap();
    assert_eq!(
        memberships.iter().filter(|org| org["name"] == name).count(),
        2
    );
    assert!(memberships
        .iter()
        .any(|org| org["id"] == first.id && org["name"] == name));
    assert!(memberships
        .iter()
        .any(|org| org["id"] == second.id && org["name"] == name));

    let response = server
        .delete(format!("/v1/organizations/{}", first.id).as_str())
        .add_header("Authorization", format!("Bearer {owner}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    for (key, expected_status) in [(&credentials[0].1, 401), (&credentials[1].1, 200)] {
        let response = server
            .get("/v1/files?limit=1")
            .add_header("Authorization", format!("Bearer {key}"))
            .await;
        assert_eq!(
            response.status_code(),
            expected_status,
            "{}",
            response.text()
        );
    }
    let workspaces = list_workspaces_with_session(&server, second.id.clone(), &owner).await;
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, credentials[1].0);
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", second.id).as_str())
        .add_header("Authorization", format!("Bearer {owner}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response
            .json::<api::routes::usage::OrganizationBalanceResponse>()
            .remaining,
        Some(20_000_000_000)
    );
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {owner}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let me = response.json::<serde_json::Value>();
    let orgs = me["organizations"].as_array().unwrap();
    assert!(!orgs.iter().any(|org| org["id"] == first.id));
    assert!(orgs
        .iter()
        .any(|org| org["id"] == second.id && org["name"] == name));
}

// ============================================
// Organization Name Reuse After Deletion Tests
// ============================================

#[tokio::test]
async fn test_organization_name_reuse_after_deletion() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _) = setup_unique_test_session(&database).await;
    let _default = create_org_with_session(&server, &owner_session).await;

    let org_name = format!("reusable-org-{}", uuid::Uuid::new_v4());

    // Step 1: Create an organization
    let create_request = api::models::CreateOrganizationRequest {
        name: org_name.clone(),
        description: Some("Original organization".to_string()),
    };

    let create_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", owner_session))
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
        .add_header("Authorization", format!("Bearer {}", owner_session))
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
        .add_header("Authorization", format!("Bearer {}", owner_session))
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;

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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Workspace names remain unique within an organization. Their conflict
    // responses must still hide database details after org labels become reusable.
    let duplicate_response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({ "name": "default" }))
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
