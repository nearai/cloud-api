// E2E tests for the admin "list organization members" endpoint
// (GET /v1/admin/organizations/{org_id}/members)

use crate::common::*;
use api::models::{ListAdminOrganizationMembersResponse, MemberRole};

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
