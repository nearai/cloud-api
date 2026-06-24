use crate::common::*;

/// Verifies the short-path DELETE /v1/organizations/{org_id}/invitations/{invitation_id}:
/// - Admin can cancel a pending invitation via the short path
/// - The invitation is no longer listed after cancellation
/// - A non-admin member gets 403
#[tokio::test]
async fn test_cancel_invitation_short_path() {
    let (server, database) = setup_test_server_with_database().await;

    // Create org (mock user becomes owner/admin)
    let org = create_org(&server).await;
    let org_id = &org.id;

    // Send an invitation via the standard invite-by-email endpoint
    let invite_response = server
        .post(format!("/v1/organizations/{org_id}/members/invite-by-email").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [
                {"email": "cancel-test@example.com", "role": "member"}
            ]
        }))
        .await;
    assert_eq!(
        invite_response.status_code(),
        200,
        "Invite-by-email should succeed: {}",
        invite_response.text()
    );
    let invite_body =
        invite_response.json::<api::models::InviteOrganizationMemberByEmailResponse>();
    assert_eq!(
        invite_body.successful, 1,
        "Invitation should have been created"
    );

    // Retrieve the invitation ID from the list endpoint (longer path)
    let list_response = server
        .get(format!("/v1/organizations/{org_id}/members/invitations").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        list_response.status_code(),
        200,
        "List invitations should succeed"
    );
    let invitations = list_response.json::<Vec<api::models::OrganizationInvitationResponse>>();
    let inv = invitations
        .iter()
        .find(|i| i.email == "cancel-test@example.com")
        .expect("Created invitation should appear in list");
    let invitation_id = &inv.id;

    // --- Non-admin path: a plain member must get 403 ---
    let (non_admin_session, _) = setup_unique_test_session(&database).await;
    // Add the new user as a member (not admin) of the org
    let org_uuid = uuid::Uuid::parse_str(org_id).unwrap();
    let non_admin_user_id = uuid::Uuid::parse_str(
        non_admin_session
            .strip_prefix("rt_")
            .unwrap_or(&non_admin_session),
    )
    .unwrap();
    {
        let pool = database.pool();
        let client = pool.get().await.expect("Failed to get database connection");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member') ON CONFLICT DO NOTHING",
                &[&org_uuid, &non_admin_user_id],
            )
            .await
            .expect("Failed to add non-admin member");
    }
    let forbidden_response = server
        .delete(format!("/v1/organizations/{org_id}/invitations/{invitation_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", non_admin_session))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        forbidden_response.status_code(),
        403,
        "Non-admin member should get 403 on short path: {}",
        forbidden_response.text()
    );

    // --- Admin path: cancel via short path ---
    let delete_response = server
        .delete(format!("/v1/organizations/{org_id}/invitations/{invitation_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        delete_response.status_code(),
        204,
        "Admin cancel via short path should return 204: {}",
        delete_response.text()
    );

    // Verify the invitation is no longer pending (list should not contain it)
    let list_after = server
        .get(format!("/v1/organizations/{org_id}/members/invitations?status=pending").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(list_after.status_code(), 200);
    let invitations_after = list_after.json::<Vec<api::models::OrganizationInvitationResponse>>();
    assert!(
        !invitations_after.iter().any(|i| i.id == *invitation_id),
        "Cancelled invitation should not appear in pending list"
    );
}

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
