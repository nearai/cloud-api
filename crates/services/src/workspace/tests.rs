use super::*;
use crate::auth::ports::{User, UserRole};
use crate::email::{
    ContactEmailVerificationEmail, EmailDeliveryOutcome, EmailError, InvitationEmail,
    ModelDeprecationEmail, PricingChangeEmail,
};
use crate::organization::{MockOrganizationServiceTrait, Organization, OrganizationMemberWithUser};
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

struct RecordingEmailSender {
    attempts: mpsc::UnboundedSender<ApiKeyChangedEmail>,
    fail: bool,
    gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl EmailSender for RecordingEmailSender {
    async fn send_invitation(
        &self,
        _: &InvitationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        unreachable!("unexpected invitation")
    }

    async fn send_model_deprecation(
        &self,
        _: &ModelDeprecationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        unreachable!("unexpected model deprecation")
    }

    async fn send_pricing_change(
        &self,
        _: &PricingChangeEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        unreachable!("unexpected pricing change")
    }

    async fn send_contact_email_verification(
        &self,
        _: &ContactEmailVerificationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        unreachable!("unexpected contact verification")
    }

    async fn send_api_key_changed(
        &self,
        email: &ApiKeyChangedEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        self.attempts.send(email.clone()).unwrap();
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.fail {
            Err(EmailError::new("simulated provider failure"))
        } else {
            Ok(EmailDeliveryOutcome::Sent { message_id: None })
        }
    }
}

fn member(role: MemberRole, active: bool, email: &str) -> OrganizationMemberWithUser {
    let id = UserId(Uuid::new_v4());
    let now = Utc::now();
    OrganizationMemberWithUser {
        organization_id: OrganizationId(Uuid::new_v4()),
        user_id: id.clone(),
        role,
        joined_at: now,
        user: User {
            id,
            email: email.into(),
            username: "member".into(),
            display_name: Some("Test member".into()),
            avatar_url: None,
            auth_provider: "github".into(),
            provider_user_id: Uuid::new_v4().to_string(),
            role: UserRole::User,
            is_active: active,
            last_login: None,
            created_at: now,
            updated_at: now,
            tokens_revoked_at: None,
            contact_email: None,
            contact_email_verified_at: None,
            email_opt_in_at: None,
        },
    }
}

fn key(creator: UserId) -> ApiKey {
    ApiKey {
        id: ApiKeyId("test-key".into()),
        key: None,
        key_prefix: "sk-safe".into(),
        name: "Deploy key".into(),
        workspace_id: WorkspaceId(Uuid::new_v4()),
        created_by_user_id: creator,
        created_at: Utc::now(),
        expires_at: None,
        last_used_at: None,
        is_active: true,
        deleted_at: None,
        spend_limit: Some(1_000_000),
        usage: None,
    }
}

fn service(
    before: &ApiKey,
    after: &ApiKey,
    revoked: bool,
    mut members: Vec<OrganizationMemberWithUser>,
    sender: Arc<RecordingEmailSender>,
    should_notify: bool,
) -> WorkspaceServiceImpl {
    let now = Utc::now();
    let organization = Organization {
        request_priority: Default::default(),
        id: OrganizationId(Uuid::new_v4()),
        name: "Test organization".into(),
        description: None,
        owner_id: members[0].user_id.clone(),
        settings: serde_json::json!({}),
        is_active: true,
        created_at: now,
        updated_at: now,
    };
    for member in &mut members {
        member.organization_id = organization.id.clone();
    }
    let workspace = Workspace {
        id: before.workspace_id.clone(),
        name: "Production".into(),
        description: None,
        organization_id: organization.id.clone(),
        created_by_user_id: organization.owner_id.clone(),
        created_at: now,
        updated_at: now,
        is_active: true,
        settings: None,
    };
    let mut workspaces = MockWorkspaceRepository::new();
    workspaces
        .expect_get_workspace_with_organization()
        .returning(move |_| Ok(Some((workspace.clone(), organization.clone()))));
    let mut keys = MockApiKeyRepository::new();
    // The authorization pre-read can be stale. Notification decisions must use
    // the previous value returned atomically by the write, not this snapshot.
    let mut existing = after.clone();
    if !should_notify {
        existing.is_active = !after.is_active;
        existing.spend_limit = Some(4_000_000);
    }
    keys.expect_get_by_id()
        .returning(move |_| Ok(Some(existing.clone())));
    let update = ApiKeyUpdate {
        previous: before.clone(),
        updated: after.clone(),
    };
    keys.expect_update_spend_limit()
        .returning(move |_, _| Ok(update.clone()));
    let update = ApiKeyUpdate {
        previous: before.clone(),
        updated: after.clone(),
    };
    keys.expect_update()
        .returning(move |_, _, _, _, _| Ok(update.clone()));
    keys.expect_revoke().returning(move |_| Ok(revoked));
    let mut organizations = MockOrganizationServiceTrait::new();
    organizations
        .expect_get_user_role()
        .returning(|_, _| Ok(Some(MemberRole::Admin)));
    let notification_count = usize::from(should_notify);
    let member_count = members.len() as i64;
    organizations
        .expect_get_member_count()
        .times(notification_count)
        .returning(move |_, _| Ok(member_count));
    organizations
        .expect_get_members_with_users_paginated()
        .times(notification_count)
        .returning(move |_, _, _, _| Ok(members.clone()));
    WorkspaceServiceImpl::new_with_email_sender(
        Arc::new(workspaces),
        Arc::new(keys),
        Arc::new(organizations),
        sender,
        Some("https://cloud.example.com".into()),
    )
}

fn sender() -> (
    Arc<RecordingEmailSender>,
    mpsc::UnboundedReceiver<ApiKeyChangedEmail>,
) {
    let (attempts, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(RecordingEmailSender {
            attempts,
            fail: false,
            gate: None,
        }),
        receiver,
    )
}

async fn receive(receiver: &mut mpsc::UnboundedReceiver<ApiKeyChangedEmail>) -> ApiKeyChangedEmail {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("notification task did not send")
        .expect("notification channel closed")
}

#[tokio::test]
async fn notification_recipients_are_active_current_members_with_responsibility() {
    for creator_role in [MemberRole::Member, MemberRole::Admin, MemberRole::Owner] {
        let creator = member(creator_role, true, "creator@example.com");
        let before = key(creator.user_id.clone());
        let owner = member(MemberRole::Owner, true, "owner@example.com");
        let requester = owner.user_id.clone();
        let mut wallet_admin = member(MemberRole::Admin, true, "wallet@near");
        wallet_admin.user.contact_email = Some("verified@example.com".into());
        wallet_admin.user.contact_email_verified_at = Some(Utc::now());
        let members = vec![
            owner,
            creator,
            wallet_admin,
            member(MemberRole::Admin, false, "inactive@example.com"),
            member(MemberRole::Owner, false, "inactive-owner@example.com"),
            member(MemberRole::Member, true, "ordinary@example.com"),
            member(MemberRole::Admin, true, "unverified@near"),
        ];
        let (sender, mut receiver) = sender();
        let service = service(&before, &before, true, members, sender, true);
        assert!(service
            .revoke_api_key(before.workspace_id, before.id, requester)
            .await
            .unwrap());
        let mut recipients = Vec::new();
        for _ in 0..3 {
            let email = receive(&mut receiver).await;
            assert_eq!(email.key_prefix, "sk-safe");
            assert_eq!(email.change, "API key revoked");
            recipients.push(email.recipient_email);
        }
        recipients.sort();
        assert_eq!(
            recipients,
            [
                "creator@example.com",
                "owner@example.com",
                "verified@example.com"
            ]
        );
        assert!(
            receiver.try_recv().is_err(),
            "creator must receive only one email"
        );
    }
}

#[tokio::test]
async fn removed_or_inactive_creator_is_not_notified() {
    for creator_is_member in [false, true] {
        let creator = member(MemberRole::Member, !creator_is_member, "former@example.com");
        let owner = member(MemberRole::Owner, true, "owner@example.com");
        let requester = owner.user_id.clone();
        let before = key(creator.user_id.clone());
        let mut members = vec![owner];
        if creator_is_member {
            members.push(creator);
        }
        let (sender, mut receiver) = sender();
        let service = service(&before, &before, true, members, sender, true);
        service
            .revoke_api_key(before.workspace_id, before.id, requester)
            .await
            .unwrap();
        assert_eq!(
            receive(&mut receiver).await.recipient_email,
            "owner@example.com"
        );
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn unchanged_limit_state_and_failed_revoke_do_not_notify() {
    for operation in 0..3 {
        let owner = member(MemberRole::Owner, true, "owner@example.com");
        let requester = owner.user_id.clone();
        let before = key(requester.clone());
        let (sender, mut receiver) = sender();
        let service = service(&before, &before, false, vec![owner], sender, false);
        match operation {
            0 => {
                service
                    .update_api_key_spend_limit(
                        before.workspace_id,
                        before.id,
                        requester,
                        before.spend_limit,
                    )
                    .await
                    .unwrap();
            }
            1 => {
                service
                    .update_api_key(
                        before.workspace_id,
                        before.id,
                        requester,
                        None,
                        None,
                        Some(before.spend_limit),
                        Some(true),
                    )
                    .await
                    .unwrap();
            }
            _ => {
                assert!(!service
                    .revoke_api_key(before.workspace_id, before.id, requester)
                    .await
                    .unwrap());
            }
        }
        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn effective_state_and_subcent_limit_changes_notify() {
    for enabled in [false, true] {
        let owner = member(MemberRole::Owner, true, "owner@example.com");
        let requester = owner.user_id.clone();
        let mut before = key(requester.clone());
        before.is_active = !enabled;
        let mut after = before.clone();
        after.is_active = enabled;
        after.spend_limit = Some(4_000_000);
        let (sender, mut receiver) = sender();
        let service = service(&before, &after, true, vec![owner], sender, true);
        let updated = service
            .update_api_key(
                before.workspace_id,
                before.id,
                requester,
                None,
                None,
                Some(after.spend_limit),
                Some(enabled),
            )
            .await
            .unwrap();
        assert_eq!(updated.is_active, enabled);
        let email = receive(&mut receiver).await;
        assert!(email.change.contains(if enabled {
            "API key enabled"
        } else {
            "API key disabled"
        }));
        assert!(email
            .change
            .contains("Spend limit changed from $0.001 to $0.004"));
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn sender_failure_does_not_fail_mutation_or_stop_other_recipients() {
    let owner = member(MemberRole::Owner, true, "owner@example.com");
    let admin = member(MemberRole::Admin, true, "admin@example.com");
    let requester = owner.user_id.clone();
    let before = key(requester.clone());
    let mut after = before.clone();
    after.spend_limit = None;
    let (attempts, mut receiver) = mpsc::unbounded_channel();
    let sender = Arc::new(RecordingEmailSender {
        attempts,
        fail: true,
        gate: None,
    });
    let service = service(&before, &after, true, vec![owner, admin], sender, true);
    let updated = service
        .update_api_key_spend_limit(before.workspace_id, before.id, requester, None)
        .await
        .unwrap();
    assert_eq!(updated.spend_limit, None);
    for expected in ["owner@example.com", "admin@example.com"] {
        let email = receive(&mut receiver).await;
        assert_eq!(email.recipient_email, expected);
        assert!(email.change.contains("$0.001 to unlimited"));
    }
}

#[tokio::test]
async fn slow_sender_does_not_block_mutation_response() {
    let owner = member(MemberRole::Owner, true, "owner@example.com");
    let requester = owner.user_id.clone();
    let before = key(requester.clone());
    let gate = Arc::new(Semaphore::new(0));
    let (attempts, mut receiver) = mpsc::unbounded_channel();
    let sender = Arc::new(RecordingEmailSender {
        attempts,
        fail: false,
        gate: Some(gate.clone()),
    });
    let service = service(&before, &before, true, vec![owner], sender, true);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        service.revoke_api_key(before.workspace_id, before.id, requester),
    )
    .await;
    assert!(result.expect("mutation waited on email delivery").unwrap());
    receive(&mut receiver).await;
    gate.add_permits(1);
}

#[test]
fn spend_limits_preserve_nano_dollar_precision() {
    for (value, expected) in [
        (None, "unlimited"),
        (Some(0), "$0.00"),
        (Some(1), "$0.000000001"),
        (Some(1_000_000), "$0.001"),
        (Some(4_000_000), "$0.004"),
        (Some(10_000_000), "$0.01"),
        (Some(1_000_000_000), "$1.00"),
        (Some(1_234_567_890), "$1.23456789"),
        (Some(i64::MAX), "$9223372036.854775807"),
        (Some(i64::MIN), "-$9223372036.854775808"),
    ] {
        assert_eq!(format_spend_limit(value), expected);
    }
}
