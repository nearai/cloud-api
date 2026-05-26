use crate::common::*;

#[tokio::test]
async fn test_user_invitations_include_organization_name() {
    let (server, database) = setup_test_server_with_database().await;
    let org_name = format!("Invitation Org {}", uuid::Uuid::new_v4());

    let create_response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&api::models::CreateOrganizationRequest {
            name: org_name.clone(),
            description: Some("Organization used for invitation name tests".to_string()),
        })
        .await;

    assert_eq!(create_response.status_code(), 200);
    let org = create_response.json::<api::models::OrganizationResponse>();
    assert_eq!(org.name, org_name);

    let invitation_id = uuid::Uuid::new_v4();
    let organization_id = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");
    let invited_by_user_id =
        uuid::Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a uuid");
    let token = format!("test-token-{}", uuid::Uuid::new_v4());
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");
    client
        .execute(
            "INSERT INTO organization_invitations (
                id,
                organization_id,
                email,
                role,
                invited_by_user_id,
                status,
                token,
                created_at,
                expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW() + INTERVAL '7 days')",
            &[
                &invitation_id,
                &organization_id,
                &"admin@test.com",
                &"member",
                &invited_by_user_id,
                &"pending",
                &token,
            ],
        )
        .await
        .expect("Failed to create invitation fixture");
    drop(client);

    let list_response = server
        .get("/v1/users/me/invitations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(list_response.status_code(), 200);
    let invitations =
        list_response.json::<Vec<api::models::OrganizationInvitationWithOrgResponse>>();
    let invitation = invitations
        .iter()
        .find(|inv| inv.invitation.id == invitation_id.to_string())
        .expect("authenticated user should have a pending invitation for the created organization");

    assert_eq!(invitation.organization_name, org.name);
    assert_eq!(invitation.invitation.email, "admin@test.com");
    assert_eq!(
        invitation.invited_by_display_name,
        Some("Test User".to_string())
    );
}
