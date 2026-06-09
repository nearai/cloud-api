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
