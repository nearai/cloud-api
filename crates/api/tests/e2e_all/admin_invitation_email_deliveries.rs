use crate::common::*;
use chrono::{Duration, SecondsFormat, Utc};

async fn insert_invitation_fixture(
    database: &std::sync::Arc<database::Database>,
    organization_id: uuid::Uuid,
    email: &str,
    status: &str,
    email_status: &str,
    created_at: chrono::DateTime<Utc>,
) -> uuid::Uuid {
    let invitation_id = uuid::Uuid::new_v4();
    let invited_by_user_id =
        uuid::Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a uuid");
    let token = format!("test-token-{}", uuid::Uuid::new_v4());
    let email_sent_at = if email_status == "sent" {
        Some(created_at + Duration::minutes(1))
    } else {
        None
    };
    let email_last_error: Option<String> = if email_status == "failed" {
        Some("Resend failed with sanitized details".to_string())
    } else {
        None
    };
    let email_message_id: Option<String> = if email_status == "sent" {
        Some(format!("resend-{invitation_id}"))
    } else {
        None
    };
    let responded_at = if status == "pending" {
        None
    } else {
        Some(created_at + Duration::minutes(5))
    };

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
                expires_at,
                responded_at,
                email_status,
                email_sent_at,
                email_last_error,
                email_message_id
            )
            VALUES ($1, $2, $3, 'member', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            &[
                &invitation_id,
                &organization_id,
                &email,
                &invited_by_user_id,
                &status,
                &token,
                &created_at,
                &(created_at + Duration::days(7)),
                &responded_at,
                &email_status,
                &email_sent_at,
                &email_last_error,
                &email_message_id,
            ],
        )
        .await
        .expect("Failed to create invitation fixture");

    invitation_id
}

#[tokio::test]
async fn test_admin_invitation_email_deliveries_filters_and_order() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let other_org = create_org(&server).await;
    let organization_id = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");
    let other_organization_id =
        uuid::Uuid::parse_str(&other_org.id).expect("org id should be a uuid");
    let now = Utc::now();

    let older_failed = insert_invitation_fixture(
        &database,
        organization_id,
        "first-invitee@example.com",
        "pending",
        "failed",
        now - Duration::hours(3),
    )
    .await;
    let newer_sent = insert_invitation_fixture(
        &database,
        organization_id,
        "second-invitee@example.com",
        "pending",
        "sent",
        now - Duration::hours(1),
    )
    .await;
    let _other_org_invitation = insert_invitation_fixture(
        &database,
        other_organization_id,
        "first-invitee@example.com",
        "pending",
        "failed",
        now,
    )
    .await;

    let list_response = server
        .get(
            format!(
                "/v1/admin/invitation-email-deliveries?organization_id={}&limit=10&offset=0",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(list_response.status_code(), 200);
    let list_body = list_response.json::<api::models::ListAdminInvitationEmailDeliveriesResponse>();
    assert_eq!(list_body.total, 2);
    assert_eq!(list_body.deliveries.len(), 2);
    assert_eq!(
        list_body.deliveries[0].invitation_id,
        newer_sent.to_string()
    );
    assert_eq!(
        list_body.deliveries[1].invitation_id,
        older_failed.to_string()
    );
    assert_eq!(list_body.deliveries[0].organization_id, org.id);
    assert_eq!(list_body.deliveries[0].organization_name, org.name);
    assert_eq!(
        list_body.deliveries[0].invited_by_email.as_deref(),
        Some("admin@test.com")
    );
    assert_eq!(
        list_body.deliveries[0].invited_by_display_name.as_deref(),
        Some("Test User")
    );

    let filtered_response = server
        .get(format!(
            "/v1/admin/invitation-email-deliveries?organization_id={}&recipient_email=FIRST&email_status=failed&invitation_status=pending",
            org.id
        ).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(filtered_response.status_code(), 200);
    let filtered_body =
        filtered_response.json::<api::models::ListAdminInvitationEmailDeliveriesResponse>();
    assert_eq!(filtered_body.total, 1);
    assert_eq!(
        filtered_body.deliveries[0].invitation_id,
        older_failed.to_string()
    );
    assert_eq!(
        filtered_body.deliveries[0].email_status,
        api::models::InvitationEmailStatus::Failed
    );
    assert_eq!(
        filtered_body.deliveries[0].email_last_error.as_deref(),
        Some("Resend failed with sanitized details")
    );

    let created_after = (now - Duration::hours(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let date_filtered_response = server
        .get(
            format!(
                "/v1/admin/invitation-email-deliveries?organization_id={}&created_after={}",
                org.id, created_after
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(date_filtered_response.status_code(), 200);
    let date_filtered_body =
        date_filtered_response.json::<api::models::ListAdminInvitationEmailDeliveriesResponse>();
    assert_eq!(date_filtered_body.total, 1);
    assert_eq!(
        date_filtered_body.deliveries[0].invitation_id,
        newer_sent.to_string()
    );
}

#[tokio::test]
async fn test_admin_invitation_email_deliveries_requires_admin_auth() {
    let server = setup_test_server().await;

    let response = server.get("/v1/admin/invitation-email-deliveries").await;

    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_admin_resend_invitation_email_rejects_non_pending() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let organization_id = uuid::Uuid::parse_str(&org.id).expect("org id should be a uuid");
    let accepted_invitation = insert_invitation_fixture(
        &database,
        organization_id,
        "accepted-invitee@example.com",
        "accepted",
        "failed",
        Utc::now(),
    )
    .await;

    let response = server
        .post(
            format!("/v1/admin/invitation-email-deliveries/{accepted_invitation}/resend").as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 400);
}
