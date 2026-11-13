// E2E tests for the admin organization member role endpoint
// (PUT /v1/admin/organizations/{org_id}/members/{user_id})

use crate::common::*;
use api::models::{MemberRole, OrganizationMemberResponse};

#[tokio::test]
async fn test_admin_updates_member_role_and_protects_owners() {
    let (server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let member_id = uuid::Uuid::new_v4();

    {
        let client = database
            .pool()
            .get()
            .await
            .expect("Failed to get database connection");
        for (user_id, label) in [(owner_id, "owner"), (member_id, "member")] {
            client
                .execute(
                    "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
                     VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW())",
                    &[
                        &user_id,
                        &format!("{label}-{user_id}@example.com"),
                        &format!("{label}-{user_id}"),
                        &format!("{label}-provider-{user_id}"),
                    ],
                )
                .await
                .expect("Failed to insert user");
        }
        client
            .execute(
                "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
                 VALUES ($1, $2, true, NOW(), NOW())",
                &[&organization_id, &format!("admin-role-{organization_id}")],
            )
            .await
            .expect("Failed to insert organization");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role)
                 VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
                &[&organization_id, &owner_id, &member_id],
            )
            .await
            .expect("Failed to insert organization members");
    }

    let update_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(
        update_response.status_code(),
        200,
        "System admin should update a member role: {}",
        update_response.text()
    );
    let member = update_response.json::<OrganizationMemberResponse>();
    assert_eq!(member.role, MemberRole::Admin);

    let demote_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "member" }))
        .await;
    assert_eq!(demote_response.status_code(), 200);
    let demoted_member = demote_response.json::<OrganizationMemberResponse>();
    assert_eq!(demoted_member.role, MemberRole::Member);

    let owner_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{owner_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "member" }))
        .await;
    assert_eq!(owner_response.status_code(), 400);

    let missing_member_response = server
        .put(
            format!(
                "/v1/admin/organizations/{organization_id}/members/{}",
                uuid::Uuid::new_v4()
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(missing_member_response.status_code(), 404);

    let no_op_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "member" }))
        .await;
    assert_eq!(no_op_response.status_code(), 200);
    assert_eq!(
        no_op_response.json::<OrganizationMemberResponse>().role,
        MemberRole::Member
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let audit_rows = client
        .query(
            "SELECT member_user_id, changed_by_user_id, previous_role, new_role
             FROM organization_member_role_audit_log
             WHERE organization_id = $1
             ORDER BY changed_at, id",
            &[&organization_id],
        )
        .await
        .expect("Failed to query member role audit log");
    assert_eq!(audit_rows.len(), 2, "Only successful changes are audited");
    let system_admin_id =
        uuid::Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a uuid");
    for row in &audit_rows {
        assert_eq!(row.get::<_, uuid::Uuid>("member_user_id"), member_id);
        assert_eq!(
            row.get::<_, uuid::Uuid>("changed_by_user_id"),
            system_admin_id
        );
    }
    assert_eq!(audit_rows[0].get::<_, String>("previous_role"), "member");
    assert_eq!(audit_rows[0].get::<_, String>("new_role"), "admin");
    assert_eq!(audit_rows[1].get::<_, String>("previous_role"), "admin");
    assert_eq!(audit_rows[1].get::<_, String>("new_role"), "member");
}

#[tokio::test]
async fn test_admin_transfers_organization_ownership_to_member() {
    let (server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let admin_id = uuid::Uuid::new_v4();
    let member_id = uuid::Uuid::new_v4();

    {
        let client = database
            .pool()
            .get()
            .await
            .expect("Failed to get database connection");
        for (user_id, label) in [
            (owner_id, "owner"),
            (admin_id, "admin"),
            (member_id, "member"),
        ] {
            client
                .execute(
                    "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
                     VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW())",
                    &[
                        &user_id,
                        &format!("{label}-{user_id}@example.com"),
                        &format!("{label}-{user_id}"),
                        &format!("{label}-provider-{user_id}"),
                    ],
                )
                .await
                .expect("Failed to insert user");
        }
        client
            .execute(
                "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
                 VALUES ($1, $2, true, NOW(), NOW())",
                &[
                    &organization_id,
                    &format!("ownership-transfer-{organization_id}"),
                ],
            )
            .await
            .expect("Failed to insert organization");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role)
                 VALUES ($1, $2, 'owner'), ($1, $3, 'admin'), ($1, $4, 'member')",
                &[&organization_id, &owner_id, &admin_id, &member_id],
            )
            .await
            .expect("Failed to insert organization members");
    }

    let response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "owner" }))
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "System admin should transfer ownership: {}",
        response.text()
    );
    assert_eq!(
        response.json::<OrganizationMemberResponse>().role,
        MemberRole::Owner
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let roles = client
        .query(
            "SELECT user_id, role FROM organization_members
             WHERE organization_id = $1",
            &[&organization_id],
        )
        .await
        .expect("Failed to query organization members");
    assert_eq!(roles.len(), 3);
    assert_eq!(
        roles
            .iter()
            .find(|row| row.get::<_, uuid::Uuid>("user_id") == owner_id)
            .expect("Previous owner should remain a member")
            .get::<_, String>("role"),
        "admin"
    );
    assert_eq!(
        roles
            .iter()
            .find(|row| row.get::<_, uuid::Uuid>("user_id") == member_id)
            .expect("Promoted member should remain a member")
            .get::<_, String>("role"),
        "owner"
    );

    let audit_rows = client
        .query(
            "SELECT member_user_id, changed_by_user_id, previous_role, new_role
             FROM organization_member_role_audit_log
             WHERE organization_id = $1",
            &[&organization_id],
        )
        .await
        .expect("Failed to query ownership transfer audit log");
    assert_eq!(audit_rows.len(), 2);
    let system_admin_id =
        uuid::Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a uuid");
    for row in &audit_rows {
        assert_eq!(
            row.get::<_, uuid::Uuid>("changed_by_user_id"),
            system_admin_id
        );
    }
    let previous_owner_audit = audit_rows
        .iter()
        .find(|row| row.get::<_, uuid::Uuid>("member_user_id") == owner_id)
        .expect("Previous owner change should be audited");
    assert_eq!(
        previous_owner_audit.get::<_, String>("previous_role"),
        "owner"
    );
    assert_eq!(previous_owner_audit.get::<_, String>("new_role"), "admin");
    let new_owner_audit = audit_rows
        .iter()
        .find(|row| row.get::<_, uuid::Uuid>("member_user_id") == member_id)
        .expect("New owner change should be audited");
    assert_eq!(new_owner_audit.get::<_, String>("previous_role"), "member");
    assert_eq!(new_owner_audit.get::<_, String>("new_role"), "owner");

    drop(client);

    let new_owner_response = server
        .patch(format!("/v1/organizations/{organization_id}/settings").as_str())
        .add_header("Authorization", format!("Bearer rt_{member_id}"))
        .json(&serde_json::json!({ "fallback_enabled": false }))
        .await;
    assert_eq!(
        new_owner_response.status_code(),
        200,
        "New owner should immediately receive owner-only permissions: {}",
        new_owner_response.text()
    );

    let previous_owner_response = server
        .patch(format!("/v1/organizations/{organization_id}/settings").as_str())
        .add_header("Authorization", format!("Bearer rt_{owner_id}"))
        .json(&serde_json::json!({ "fallback_enabled": true }))
        .await;
    assert_eq!(
        previous_owner_response.status_code(),
        403,
        "Previous owner should immediately lose owner-only permissions: {}",
        previous_owner_response.text()
    );

    for (user_id, expected_role) in [
        (member_id, MemberRole::Owner),
        (owner_id, MemberRole::Admin),
    ] {
        let response = server
            .get(format!("/v1/organizations/{organization_id}").as_str())
            .add_header("Authorization", format!("Bearer rt_{user_id}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let organization = response.json::<api::models::OrganizationResponse>();
        assert_eq!(organization.owner_id, member_id.to_string());
        assert_eq!(organization.role, expected_role);
    }

    let previous_owner_role_update = server
        .put(format!("/v1/organizations/{organization_id}/members/{admin_id}").as_str())
        .add_header("Authorization", format!("Bearer rt_{owner_id}"))
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(
        previous_owner_role_update.status_code(),
        403,
        "Previous owner should not retain owner-only role management: {}",
        previous_owner_role_update.text()
    );

    let new_owner_role_update = server
        .put(format!("/v1/organizations/{organization_id}/members/{admin_id}").as_str())
        .add_header("Authorization", format!("Bearer rt_{member_id}"))
        .json(&serde_json::json!({ "role": "member" }))
        .await;
    assert_eq!(
        new_owner_role_update.status_code(),
        200,
        "New owner should be able to manage member roles: {}",
        new_owner_role_update.text()
    );

    let previous_owner_delete = server
        .delete(format!("/v1/organizations/{organization_id}").as_str())
        .add_header("Authorization", format!("Bearer rt_{owner_id}"))
        .await;
    assert_eq!(
        previous_owner_delete.status_code(),
        403,
        "Previous owner should not retain organization deletion permission: {}",
        previous_owner_delete.text()
    );

    let new_owner_delete = server
        .delete(format!("/v1/organizations/{organization_id}").as_str())
        .add_header("Authorization", format!("Bearer rt_{member_id}"))
        .await;
    assert_eq!(
        new_owner_delete.status_code(),
        409,
        "Ownership transfer must not allow deletion of a member's default organization: {}",
        new_owner_delete.text()
    );
}

#[tokio::test]
async fn test_admin_member_role_updates_reject_unknown_and_inactive_organizations() {
    let (server, database) = setup_test_server_with_database().await;
    let inactive_organization_id = uuid::Uuid::new_v4();
    let member_id = uuid::Uuid::new_v4();

    {
        let client = database
            .pool()
            .get()
            .await
            .expect("Failed to get database connection");
        client
            .execute(
                "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
                 VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW())",
                &[
                    &member_id,
                    &format!("inactive-org-member-{member_id}@example.com"),
                    &format!("inactive-org-member-{member_id}"),
                    &format!("inactive-org-member-provider-{member_id}"),
                ],
            )
            .await
            .expect("Failed to insert member user");
        client
            .execute(
                "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
                 VALUES ($1, $2, false, NOW(), NOW())",
                &[
                    &inactive_organization_id,
                    &format!("inactive-admin-role-{inactive_organization_id}"),
                ],
            )
            .await
            .expect("Failed to insert inactive organization");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role)
                 VALUES ($1, $2, 'member')",
                &[&inactive_organization_id, &member_id],
            )
            .await
            .expect("Failed to insert inactive organization member");
    }

    for organization_id in [inactive_organization_id, uuid::Uuid::new_v4()] {
        let response = server
            .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({ "role": "admin" }))
            .await;
        assert_eq!(
            response.status_code(),
            404,
            "Inactive and unknown organizations should reject role updates"
        );
    }
}

#[tokio::test]
async fn test_admin_member_role_updates_reject_malformed_role() {
    let server = setup_test_server().await;
    let response = server
        .put(
            format!(
                "/v1/admin/organizations/{}/members/{}",
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4()
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "administrator" }))
        .await;
    assert_eq!(response.status_code(), 422);
}

#[tokio::test]
async fn test_admin_member_role_updates_require_authentication() {
    let organization_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();
    let server = setup_test_server().await;

    let response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{user_id}").as_str())
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_admin_member_role_updates_reject_non_admin_users() {
    let server = setup_test_server_with_config(|config| {
        config.auth.admin_emails = vec![NON_ADMIN_ALLOWLIST_EMAIL.to_string()];
    })
    .await;
    let organization_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();

    let response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{user_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(response.status_code(), 403);
}

#[tokio::test]
async fn test_ownership_transfer_racing_target_removal_preserves_owner() {
    let (_server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();

    let mut blocker = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    blocker
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
             VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW()),
                    ($5, $6, $7, 'mock', $8, true, NOW(), NOW())",
            &[
                &owner_id,
                &format!("race-owner-{owner_id}@example.com"),
                &format!("race-owner-{owner_id}"),
                &format!("race-owner-provider-{owner_id}"),
                &target_id,
                &format!("race-target-{target_id}@example.com"),
                &format!("race-target-{target_id}"),
                &format!("race-target-provider-{target_id}"),
            ],
        )
        .await
        .expect("Failed to insert users");
    blocker
        .execute(
            "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
             VALUES ($1, $2, true, NOW(), NOW())",
            &[&organization_id, &format!("removal-race-{organization_id}")],
        )
        .await
        .expect("Failed to insert organization");
    blocker
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'admin')",
            &[&organization_id, &owner_id, &target_id],
        )
        .await
        .expect("Failed to insert memberships");

    let transaction = blocker
        .transaction()
        .await
        .expect("Failed to begin blocking transaction");
    transaction
        .query_one(
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("Failed to lock organization");

    let removal_task = {
        let pool = database.pool().clone();
        tokio::spawn(async move {
            let repository = database::repositories::PgOrganizationRepository::new(pool);
            services::organization::OrganizationRepository::remove_member(
                &repository,
                organization_id,
                target_id,
                owner_id,
            )
            .await
        })
    };

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert!(
        !removal_task.is_finished(),
        "member removal should wait for the ownership transaction"
    );

    transaction
        .execute(
            "UPDATE organization_members SET role = CASE
                 WHEN user_id = $2 THEN 'admin'
                 WHEN user_id = $3 THEN 'owner'
                 ELSE role
             END
             WHERE organization_id = $1 AND user_id IN ($2, $3)",
            &[&organization_id, &owner_id, &target_id],
        )
        .await
        .expect("Failed to transfer ownership");
    transaction
        .commit()
        .await
        .expect("Failed to commit ownership transfer");

    let removal_result = removal_task
        .await
        .expect("Removal task should not panic")
        .expect("Removal authorization check should complete");
    assert_eq!(
        removal_result,
        services::organization::RemoveOrganizationMemberResult::LastOwner
    );

    let row = database
        .pool()
        .get()
        .await
        .expect("Failed to get verification connection")
        .query_one(
            "SELECT role, (SELECT COUNT(*) FROM organization_members
                           WHERE organization_id = $1 AND role = 'owner') AS owner_count
             FROM organization_members
             WHERE organization_id = $1 AND user_id = $2",
            &[&organization_id, &target_id],
        )
        .await
        .expect("New owner membership should remain");
    assert_eq!(row.get::<_, String>("role"), "owner");
    assert_eq!(row.get::<_, i64>("owner_count"), 1);
}

#[tokio::test]
async fn test_member_removal_racing_requester_demotion_rechecks_authorization() {
    let (_server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let requester_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();

    let mut blocker = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    for (user_id, label) in [
        (owner_id, "removal-owner"),
        (requester_id, "removal-requester"),
        (target_id, "removal-target"),
    ] {
        blocker
            .execute(
                "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
                 VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW())",
                &[
                    &user_id,
                    &format!("{label}-{user_id}@example.com"),
                    &format!("{label}-{user_id}"),
                    &format!("{label}-provider-{user_id}"),
                ],
            )
            .await
            .expect("Failed to insert user");
    }
    blocker
        .execute(
            "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
             VALUES ($1, $2, true, NOW(), NOW())",
            &[
                &organization_id,
                &format!("removal-auth-race-{organization_id}"),
            ],
        )
        .await
        .expect("Failed to insert organization");
    blocker
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'admin'), ($1, $4, 'member')",
            &[&organization_id, &owner_id, &requester_id, &target_id],
        )
        .await
        .expect("Failed to insert memberships");

    let transaction = blocker
        .transaction()
        .await
        .expect("Failed to begin blocking transaction");
    transaction
        .query_one(
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("Failed to lock organization");

    let removal_task = {
        let pool = database.pool().clone();
        tokio::spawn(async move {
            let repository = database::repositories::PgOrganizationRepository::new(pool);
            services::organization::OrganizationRepository::remove_member(
                &repository,
                organization_id,
                target_id,
                requester_id,
            )
            .await
        })
    };

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert!(
        !removal_task.is_finished(),
        "member removal should wait for the role-change transaction"
    );

    transaction
        .execute(
            "UPDATE organization_members SET role = 'member'
             WHERE organization_id = $1 AND user_id = $2",
            &[&organization_id, &requester_id],
        )
        .await
        .expect("Failed to demote requester");
    transaction
        .commit()
        .await
        .expect("Failed to commit requester demotion");

    let removal_result = removal_task
        .await
        .expect("Removal task should not panic")
        .expect("Removal authorization check should complete");
    assert_eq!(
        removal_result,
        services::organization::RemoveOrganizationMemberResult::Unauthorized
    );

    let target_exists = database
        .pool()
        .get()
        .await
        .expect("Failed to get verification connection")
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM organization_members
                 WHERE organization_id = $1 AND user_id = $2
             ) AS target_exists",
            &[&organization_id, &target_id],
        )
        .await
        .expect("Failed to verify target membership")
        .get::<_, bool>("target_exists");
    assert!(target_exists);
}

#[tokio::test]
async fn test_ownership_transfer_racing_old_owner_delete_rechecks_authorization() {
    let (_server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let new_owner_id = uuid::Uuid::new_v4();

    let mut blocker = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    blocker
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active, created_at, updated_at)
             VALUES ($1, $2, $3, 'mock', $4, true, NOW(), NOW()),
                    ($5, $6, $7, 'mock', $8, true, NOW(), NOW())",
            &[
                &owner_id,
                &format!("delete-race-owner-{owner_id}@example.com"),
                &format!("delete-race-owner-{owner_id}"),
                &format!("delete-race-owner-provider-{owner_id}"),
                &new_owner_id,
                &format!("delete-race-target-{new_owner_id}@example.com"),
                &format!("delete-race-target-{new_owner_id}"),
                &format!("delete-race-target-provider-{new_owner_id}"),
            ],
        )
        .await
        .expect("Failed to insert users");
    blocker
        .execute(
            "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
             VALUES ($1, $2, true, NOW(), NOW())",
            &[&organization_id, &format!("delete-race-{organization_id}")],
        )
        .await
        .expect("Failed to insert organization");
    blocker
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
            &[&organization_id, &owner_id, &new_owner_id],
        )
        .await
        .expect("Failed to insert memberships");

    let transaction = blocker
        .transaction()
        .await
        .expect("Failed to begin blocking transaction");
    transaction
        .query_one(
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("Failed to lock organization");

    let deletion_task = {
        let pool = database.pool().clone();
        tokio::spawn(async move {
            let repository = database::repositories::PgOrganizationRepository::new(pool);
            services::organization::OrganizationRepository::delete_if_no_staking_farm_source(
                &repository,
                organization_id,
                owner_id,
            )
            .await
        })
    };

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert!(
        !deletion_task.is_finished(),
        "organization deletion should wait for the ownership transaction"
    );

    transaction
        .execute(
            "UPDATE organization_members SET role = CASE
                 WHEN user_id = $2 THEN 'admin'
                 WHEN user_id = $3 THEN 'owner'
                 ELSE role
             END
             WHERE organization_id = $1 AND user_id IN ($2, $3)",
            &[&organization_id, &owner_id, &new_owner_id],
        )
        .await
        .expect("Failed to transfer ownership");
    transaction
        .commit()
        .await
        .expect("Failed to commit ownership transfer");

    let deletion_result = deletion_task
        .await
        .expect("Deletion task should not panic")
        .expect("Deletion authorization check should complete");
    assert_eq!(
        deletion_result,
        services::organization::DeleteOrganizationResult::Unauthorized
    );

    let is_active = database
        .pool()
        .get()
        .await
        .expect("Failed to get verification connection")
        .query_one(
            "SELECT is_active FROM organizations WHERE id = $1",
            &[&organization_id],
        )
        .await
        .expect("Organization should remain")
        .get::<_, bool>("is_active");
    assert!(is_active);
}
