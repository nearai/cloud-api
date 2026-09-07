// E2E tests for the admin "list organization members" endpoint
// (GET /v1/admin/organizations/{org_id}/members)

use crate::common::*;
use api::models::{
    InviteOrganizationMemberByEmailResponse, ListAdminOrganizationMembersResponse, MemberRole,
    OrganizationMemberResponse,
};

#[tokio::test]
async fn test_admin_list_organization_members_includes_owner() {
    let server = setup_test_server().await;

    // create_org makes the current session user the owner (and thus a member).
    let org = create_org(&server).await;

    let response = server
        .get(format!("/v1/admin/organizations/{}/members", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully list organization members, body: {}",
        response.text()
    );

    let body = response.json::<ListAdminOrganizationMembersResponse>();

    assert!(
        body.total >= 1,
        "Org should have at least its owner as a member, got total={}",
        body.total
    );
    assert!(!body.members.is_empty(), "Members list should not be empty");

    let owner = body
        .members
        .iter()
        .find(|m| m.role == MemberRole::Owner)
        .expect("Owner should be present in the member list");

    // Admin view exposes full user details (unlike the member-facing endpoint).
    assert!(
        !owner.user.email.is_empty(),
        "Admin member view should expose the user's email"
    );
    assert_eq!(
        owner.organization_id, org.id,
        "Member organization_id should match the requested org"
    );

    println!("✅ Admin list organization members returns the owner with full details");
}

#[tokio::test]
async fn test_admin_list_organization_members_includes_inactive() {
    // Inactive (soft-deleted) users are deactivated in place (users.is_active =
    // false). The admin member list must still surface them — matching
    // /v1/admin/users — so totals don't silently disagree with the row set.
    let (server, database) = setup_test_server_with_database().await;

    // The mock admin user is the org owner (an active member).
    let org = create_org(&server).await;
    let org_uuid = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");

    // Insert a second user that is INACTIVE and add them as a member directly.
    let inactive_user_id = uuid::Uuid::new_v4();
    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, is_active, created_at, updated_at)
                 VALUES ($1, $2, $3, NULL, NULL, 'mock', $4, false, NOW(), NOW())",
                &[
                    &inactive_user_id,
                    &format!("inactive-{inactive_user_id}@test.com"),
                    &format!("inactive-{inactive_user_id}"),
                    &format!("mock_inactive-{inactive_user_id}"),
                ],
            )
            .await
            .expect("Failed to insert inactive user");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member')",
                &[&org_uuid, &inactive_user_id],
            )
            .await
            .expect("Failed to insert inactive member");
    }

    let response = server
        .get(format!("/v1/admin/organizations/{}/members", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should list members, body: {}",
        response.text()
    );

    let body = response.json::<ListAdminOrganizationMembersResponse>();

    // Both the active owner and the inactive member must be counted and listed.
    assert_eq!(
        body.total, 2,
        "Owner + inactive member should both be counted, got total={}",
        body.total
    );
    let inactive = body
        .members
        .iter()
        .find(|m| m.user.id == inactive_user_id.to_string())
        .expect("Inactive member should be present in the admin list");
    assert!(
        !inactive.user.is_active,
        "Inactive member should report is_active=false"
    );

    println!("✅ Admin list organization members includes inactive (soft-deleted) members");
}

#[tokio::test]
async fn test_admin_list_organization_members_empty_org() {
    // An active org with zero members returns 200 with an empty list (not 404).
    // This is the path that actually exercises `organization_exists`.
    let (server, database) = setup_test_server_with_database().await;

    // Insert an active org directly, with no members.
    let empty_org_id = uuid::Uuid::new_v4();
    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
                 VALUES ($1, $2, true, NOW(), NOW())",
                &[&empty_org_id, &format!("empty-org-{empty_org_id}")],
            )
            .await
            .expect("Failed to insert empty org");
    }

    let response = server
        .get(format!("/v1/admin/organizations/{empty_org_id}/members").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Active org with no members should return 200, body: {}",
        response.text()
    );
    let body = response.json::<ListAdminOrganizationMembersResponse>();
    assert_eq!(body.total, 0, "Empty org should report total=0");
    assert!(
        body.members.is_empty(),
        "Empty org should return an empty member list"
    );

    let invite_response = server
        .post(format!("/v1/admin/organizations/{empty_org_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{ "email": "first-member@example.com", "role": "admin" }]
        }))
        .await;
    assert_eq!(
        invite_response.status_code(),
        400,
        "Inviting into an ownerless org should fail before creating invitations: {}",
        invite_response.text()
    );
    assert!(invite_response.text().contains("Organization has no owner"));
    let client = database.pool().get().await.unwrap();
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_invitations WHERE organization_id = $1",
            &[&empty_org_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);

    println!("✅ Admin list organization members returns 200/empty for an active member-less org");
}

#[tokio::test]
async fn test_admin_list_organization_members_deactivated_org() {
    // A soft-deleted org always 404s — even though its member rows survive the
    // soft delete — matching /v1/admin/organizations, which hides inactive orgs.
    let (server, database) = setup_test_server_with_database().await;

    let org = create_org(&server).await;
    let org_uuid = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");

    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "UPDATE organizations SET is_active = false WHERE id = $1",
                &[&org_uuid],
            )
            .await
            .expect("Failed to deactivate org");
    }

    let response = server
        .get(format!("/v1/admin/organizations/{}/members", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Deactivated org should 404 regardless of surviving member rows, body: {}",
        response.text()
    );

    println!("✅ Admin list organization members 404s for a deactivated org");
}

#[tokio::test]
async fn test_admin_list_organization_members_pagination() {
    // A two-member org paged with limit=1 must return each member exactly once
    // across the two pages — locking in the `joined_at DESC, m.id` tiebreaker.
    let (server, database) = setup_test_server_with_database().await;

    let org = create_org(&server).await; // owner = member #1
    let org_uuid = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");

    // Add a second active member directly.
    let second_user_id = uuid::Uuid::new_v4();
    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, is_active, created_at, updated_at)
                 VALUES ($1, $2, $3, NULL, NULL, 'mock', $4, true, NOW(), NOW())",
                &[
                    &second_user_id,
                    &format!("second-{second_user_id}@test.com"),
                    &format!("second-{second_user_id}"),
                    &format!("mock_second-{second_user_id}"),
                ],
            )
            .await
            .expect("Failed to insert second user");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member')",
                &[&org_uuid, &second_user_id],
            )
            .await
            .expect("Failed to insert second member");
    }

    let page0 = server
        .get(
            format!(
                "/v1/admin/organizations/{}/members?limit=1&offset=0",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<ListAdminOrganizationMembersResponse>();
    let page1 = server
        .get(
            format!(
                "/v1/admin/organizations/{}/members?limit=1&offset=1",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<ListAdminOrganizationMembersResponse>();

    assert_eq!(
        page0.total, 2,
        "total should reflect the full count on page 0"
    );
    assert_eq!(
        page1.total, 2,
        "total should reflect the full count on page 1"
    );
    assert_eq!(page0.members.len(), 1, "limit=1 should return one row");
    assert_eq!(page1.members.len(), 1, "limit=1 should return one row");
    assert_ne!(
        page0.members[0].id, page1.members[0].id,
        "the two pages must not repeat the same member (stable sort tiebreaker)"
    );

    println!("✅ Admin list organization members paginates without repeats/skips");
}

#[tokio::test]
async fn test_admin_list_organization_members_org_not_found() {
    let server = setup_test_server().await;

    let fake_org_id = uuid::Uuid::new_v4();
    let response = server
        .get(format!("/v1/admin/organizations/{fake_org_id}/members").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Listing members of a non-existent org should return 404, body: {}",
        response.text()
    );

    println!("✅ Admin list organization members returns 404 for unknown org");
}

#[tokio::test]
async fn test_admin_get_organization_ok_and_not_found() {
    let (server, database) = setup_test_server_with_database().await;

    // Existing active org -> 200 with matching id/name.
    let org = create_org(&server).await;
    let response = server
        .get(format!("/v1/admin/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "Should get the org, body: {}",
        response.text()
    );
    let body = response.json::<api::models::AdminOrganizationResponse>();
    assert_eq!(body.id, org.id, "Returned org id should match");

    // Unknown org -> 404.
    let fake_org_id = uuid::Uuid::new_v4();
    let response = server
        .get(format!("/v1/admin/organizations/{fake_org_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 404, "Unknown org should 404");

    // Deactivated org -> 404 (consistent with the org list hiding inactive orgs).
    let org_uuid = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");
    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "UPDATE organizations SET is_active = false WHERE id = $1",
                &[&org_uuid],
            )
            .await
            .expect("Failed to deactivate org");
    }
    let response = server
        .get(format!("/v1/admin/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        404,
        "Deactivated org should 404, body: {}",
        response.text()
    );

    println!("✅ Admin get organization returns 200 / 404 / 404 (deactivated)");
}

#[tokio::test]
async fn test_admin_list_organization_members_unauthorized() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let response = server
        .get(format!("/v1/admin/organizations/{}/members", org.id).as_str())
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Listing members without auth should require authentication"
    );

    println!("✅ Admin list organization members correctly requires authentication");
}

#[tokio::test]
async fn test_admin_invites_member_without_organization_membership() {
    let (server, database) = setup_test_server_with_database().await;
    let organization_id = uuid::Uuid::new_v4();
    let owner_id = uuid::Uuid::new_v4();
    let invited_email = format!("invite-{organization_id}@example.com");

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
                    &owner_id,
                    &format!("owner-{owner_id}@example.com"),
                    &format!("owner-{owner_id}"),
                    &format!("owner-provider-{owner_id}"),
                ],
            )
            .await
            .expect("Failed to insert organization owner");
        client
            .execute(
                "INSERT INTO organizations (id, name, is_active, created_at, updated_at)
                 VALUES ($1, $2, true, NOW(), NOW())",
                &[&organization_id, &format!("admin-invite-{organization_id}")],
            )
            .await
            .expect("Failed to insert organization");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role)
                 VALUES ($1, $2, 'owner')",
                &[&organization_id, &owner_id],
            )
            .await
            .expect("Failed to insert organization owner membership");
    }

    let response = server
        .post(format!("/v1/admin/organizations/{organization_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{ "email": invited_email.to_uppercase(), "role": "admin" }]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "System admin should be able to invite without org membership: {}",
        response.text()
    );
    let body = response.json::<InviteOrganizationMemberByEmailResponse>();
    assert_eq!(body.successful, 1);
    assert_eq!(body.failed, 0);

    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let row = client
        .query_one(
            "SELECT role, invited_by_user_id, status
             FROM organization_invitations
             WHERE organization_id = $1 AND email = $2",
            &[&organization_id, &invited_email],
        )
        .await
        .expect("Invitation should be persisted");
    assert_eq!(row.get::<_, String>("role"), "admin");
    assert_eq!(
        row.get::<_, uuid::Uuid>("invited_by_user_id").to_string(),
        MOCK_USER_ID
    );
    assert_eq!(row.get::<_, String>("status"), "pending");

    for email in [invited_email.clone(), invited_email.to_uppercase()] {
        let repeated_response = server
            .post(
                format!("/v1/admin/organizations/{organization_id}/members/invite-by-email")
                    .as_str(),
            )
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({
                "invitations": [{ "email": email, "role": "admin" }]
            }))
            .await;
        assert_eq!(
            repeated_response.status_code(),
            200,
            "Repeated invitation should succeed: {}",
            repeated_response.text()
        );
    }

    let status_counts = client
        .query_one(
            "SELECT
                 COUNT(*) FILTER (WHERE status = 'pending') AS pending_count,
                 COUNT(*) FILTER (WHERE status = 'expired') AS expired_count
             FROM organization_invitations
             WHERE organization_id = $1 AND LOWER(email) = LOWER($2)",
            &[&organization_id, &invited_email],
        )
        .await
        .expect("Invitation history should be queryable");
    assert_eq!(status_counts.get::<_, i64>("pending_count"), 1);
    assert_eq!(status_counts.get::<_, i64>("expired_count"), 2);

    let owner_invite_response = server
        .post(format!("/v1/admin/organizations/{organization_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{ "email": "owner-invite@example.com", "role": "owner" }]
        }))
        .await;
    assert_eq!(owner_invite_response.status_code(), 400);
}

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

    let existing_member_invite = server
        .post(format!("/v1/admin/organizations/{organization_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{
                "email": format!("member-{member_id}@example.com").to_uppercase(),
                "role": "member"
            }]
        }))
        .await;
    assert_eq!(existing_member_invite.status_code(), 200);
    let existing_member_result =
        existing_member_invite.json::<InviteOrganizationMemberByEmailResponse>();
    assert_eq!(existing_member_result.successful, 0);
    assert_eq!(existing_member_result.failed, 1);
    assert_eq!(
        existing_member_result.results[0].error.as_deref(),
        Some("User is already a member")
    );

    let owner_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{owner_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "member" }))
        .await;
    assert_eq!(owner_response.status_code(), 400);

    let promote_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{member_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "owner" }))
        .await;
    assert_eq!(promote_response.status_code(), 400);
}

#[tokio::test]
async fn test_admin_member_writes_require_authentication() {
    let organization_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();
    let server = setup_test_server().await;

    let invite_response = server
        .post(format!("/v1/admin/organizations/{organization_id}/members/invite-by-email").as_str())
        .json(&serde_json::json!({
            "invitations": [{ "email": "unauthorized@example.com", "role": "member" }]
        }))
        .await;
    assert_eq!(invite_response.status_code(), 401);

    let update_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{user_id}").as_str())
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(update_response.status_code(), 401);
}

#[tokio::test]
async fn test_admin_member_writes_reject_non_admin_users() {
    let server = setup_test_server_with_config(|config| {
        config.auth.admin_domains = vec!["example.org".to_string()];
    })
    .await;
    let organization_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();

    let invite_response = server
        .post(format!("/v1/admin/organizations/{organization_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{ "email": "forbidden@example.com", "role": "member" }]
        }))
        .await;
    assert_eq!(invite_response.status_code(), 403);

    let update_response = server
        .put(format!("/v1/admin/organizations/{organization_id}/members/{user_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "role": "admin" }))
        .await;
    assert_eq!(update_response.status_code(), 403);
}

#[tokio::test]
async fn test_admin_rejects_invitation_when_members_exist_without_owner() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organization_members SET role = 'admin' WHERE organization_id = $1",
            &[&org_id],
        )
        .await
        .unwrap();
    let response = server
        .post(format!("/v1/admin/organizations/{org_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{"email": "ownerless@example.com", "role": "member"}]
        }))
        .await;
    assert_eq!(response.status_code(), 400);
    assert!(response.text().contains("Organization has no owner"));
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_invitations WHERE organization_id = $1",
            &[&org_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_admin_invitation_ignores_inactive_members_but_checks_active_same_email() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let inactive_id = uuid::Uuid::new_v4();
    let active_id = uuid::Uuid::new_v4();
    let email = format!("inactive-{inactive_id}@example.com");
    let client = database.pool().get().await.unwrap();
    for (id, address, active) in [
        (inactive_id, email.clone(), false),
        (active_id, email.to_uppercase(), true),
    ] {
        client.execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id, is_active)
             VALUES ($1, $2, $3, 'mock', $3, $4)",
            &[&id, &address, &id.to_string(), &active],
        ).await.unwrap();
    }
    client.execute(
        "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member')",
        &[&org_id, &inactive_id],
    ).await.unwrap();

    for active_is_member in [false, true] {
        if active_is_member {
            client.execute(
                "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member')",
                &[&org_id, &active_id],
            ).await.unwrap();
        }
        let response = server
            .post(format!("/v1/admin/organizations/{org_id}/members/invite-by-email").as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({
                "invitations": [{"email": email, "role": "member"}]
            }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let body = response.json::<InviteOrganizationMemberByEmailResponse>();
        if active_is_member {
            assert_eq!((body.successful, body.failed), (0, 1));
            assert_eq!(
                body.results[0].error.as_deref(),
                Some("User is already a member")
            );
        } else {
            assert_eq!((body.successful, body.failed), (1, 0));
        }
    }
    let active: bool = client
        .query_one("SELECT is_active FROM users WHERE id = $1", &[&inactive_id])
        .await
        .unwrap()
        .get(0);
    assert!(!active, "Inviting does not reactivate the old account");
}

#[tokio::test]
async fn test_admin_invitation_duplicate_batch_creates_only_one_record() {
    let (server, database) = setup_test_server_with_database().await;
    for admin_prefix in ["/v1/admin", "/v1"] {
        let org = create_org(&server).await;
        let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
        let response = server
            .post(format!("{admin_prefix}/organizations/{org_id}/members/invite-by-email").as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({"invitations": [
                {"email": "Duplicate@Example.com", "role": "member"},
                {"email": "duplicate@example.com", "role": "admin"}
            ]}))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let body = response.json::<InviteOrganizationMemberByEmailResponse>();
        assert_eq!((body.total, body.successful, body.failed), (2, 1, 1));
        assert_eq!(
            body.results[1].error.as_deref(),
            Some("Duplicate email in invitation batch")
        );
        let client = database.pool().get().await.unwrap();
        let rows = client
            .query(
                "SELECT role, status FROM organization_invitations WHERE organization_id = $1",
                &[&org_id],
            )
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "Duplicate must not expire and replace the first invitation"
        );
        assert_eq!(rows[0].get::<_, String>("role"), "member");
        assert_eq!(rows[0].get::<_, String>("status"), "pending");
    }
}
