use super::*;

fn service_with_update(
    previous_role: MemberRole,
    new_role: MemberRole,
    additional_changes: Vec<OrganizationMemberRoleChange>,
    admin: bool,
    outcome: Result<EmailDeliveryOutcome, EmailError>,
) -> (
    OrganizationServiceImpl,
    Arc<StubEmailSender>,
    Organization,
    UserId,
    Arc<StubUserRepo>,
) {
    let (mut service, _, sender, users) = make_service(
        outcome,
        Some("https://cloud.example.com/dashboard/invitations".into()),
    );
    let actor_id = users.inviter.id.clone();
    let org = Organization {
        request_priority: 0,
        id: OrganizationId(Uuid::new_v4()),
        name: "Role change organization".into(),
        description: None,
        owner_id: actor_id.clone(),
        settings: serde_json::json!({}),
        is_active: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let member_id = UserId(Uuid::new_v4());
    let notification_users = std::iter::once(member_id.clone())
        .chain(
            additional_changes
                .iter()
                .map(|change| change.user_id.clone()),
        )
        .map(|user_id| {
            let mut user = users.inviter.clone();
            user.email = format!("{}@example.test", user_id.0);
            user.id = user_id.clone();
            (user_id, user)
        })
        .collect();
    let notification_users = Arc::new(StubUserRepo {
        inviter: users.inviter.clone(),
        get_by_id_calls: Mutex::new(0),
        notification_users,
    });
    service.user_repository = notification_users.clone();
    let mut repository = MockOrganizationRepository::new();
    let returned_org = org.clone();
    repository
        .expect_get_by_id()
        .times(1)
        .returning(move |_| Ok(Some(returned_org.clone())));
    // No get_member expectation: the email must use the transaction's previous
    // role, rather than taking another snapshot before the write.
    let update = OrganizationMemberRoleUpdate {
        member: OrganizationMember {
            organization_id: org.id.clone(),
            user_id: member_id.clone(),
            role: new_role,
            joined_at: chrono::Utc::now(),
        },
        previous_role,
        additional_changes,
    };
    let expected_org_id = org.id.0;
    let expected_member_id = member_id.0;
    let expected_actor_id = actor_id.0;
    if admin {
        repository
            .expect_update_member_role_with_audit()
            .times(1)
            .returning(move |org_id, member_id, request, actor_id| {
                assert_eq!(
                    (org_id, member_id, actor_id),
                    (expected_org_id, expected_member_id, expected_actor_id)
                );
                assert_eq!(request.role, update.member.role);
                Ok(update.clone())
            });
    } else {
        repository.expect_update_member().times(1).returning(
            move |org_id, member_id, request, actor_id| {
                assert_eq!(
                    (org_id, member_id, actor_id),
                    (expected_org_id, expected_member_id, expected_actor_id)
                );
                assert_eq!(request.role, update.member.role);
                Ok(update.clone())
            },
        );
    }
    service.repository = Arc::new(repository);
    (service, sender, org, member_id, notification_users)
}

async fn wait_for_emails(sender: &StubEmailSender, count: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let notified = sender.permission_attempted.notified();
            if sender.permission_changes.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("notification tasks should attempt every email");
}

#[tokio::test]
async fn owner_managed_role_email_uses_transaction_result() {
    let (service, sender, org, member_id, _) = service_with_update(
        MemberRole::Admin,
        MemberRole::Member,
        Vec::new(),
        false,
        Ok(EmailDeliveryOutcome::Sent { message_id: None }),
    );
    let updated = service
        .update_member_role(org.id.clone(), org.owner_id, member_id, MemberRole::Member)
        .await
        .unwrap();
    assert_eq!(updated.role, MemberRole::Member);
    wait_for_emails(&sender, 1).await;
    let emails = sender.permission_changes.lock().unwrap();
    assert_eq!(emails.len(), 1);
    assert_eq!(
        (
            emails[0].previous_role.as_str(),
            emails[0].new_role.as_str()
        ),
        ("admin", "member")
    );
    assert_eq!(emails[0].organization_name, org.name);
    assert_eq!(
        emails[0].members_url,
        format!(
            "https://cloud.example.com/dashboard/organizations/{}/members",
            org.id.0
        )
    );
}

#[tokio::test]
async fn ownership_transfer_emails_new_and_demoted_owners() {
    let demoted_owner_id = UserId(Uuid::new_v4());
    let (service, sender, org, member_id, _) = service_with_update(
        MemberRole::Member,
        MemberRole::Owner,
        vec![OrganizationMemberRoleChange {
            user_id: demoted_owner_id.clone(),
            previous_role: MemberRole::Owner,
            new_role: MemberRole::Admin,
        }],
        true,
        Ok(EmailDeliveryOutcome::Sent { message_id: None }),
    );
    service
        .update_member_role_for_admin(org.id, member_id.clone(), MemberRole::Owner, org.owner_id)
        .await
        .unwrap();
    wait_for_emails(&sender, 2).await;
    let emails = sender.permission_changes.lock().unwrap();
    assert_eq!(emails.len(), 2);
    let new_owner = emails
        .iter()
        .find(|email| email.recipient_email == format!("{}@example.test", member_id.0))
        .unwrap();
    let demoted_owner = emails
        .iter()
        .find(|email| email.recipient_email == format!("{}@example.test", demoted_owner_id.0))
        .unwrap();
    assert_eq!(
        (
            new_owner.previous_role.as_str(),
            new_owner.new_role.as_str()
        ),
        ("member", "owner")
    );
    assert_eq!(
        (
            demoted_owner.previous_role.as_str(),
            demoted_owner.new_role.as_str()
        ),
        ("owner", "admin")
    );
}

#[tokio::test]
async fn unchanged_role_sends_no_email_on_either_update_path() {
    for admin in [false, true] {
        let (service, sender, org, member_id, users) = service_with_update(
            MemberRole::Admin,
            MemberRole::Admin,
            Vec::new(),
            admin,
            Ok(EmailDeliveryOutcome::Sent { message_id: None }),
        );
        if admin {
            service
                .update_member_role_for_admin(org.id, member_id, MemberRole::Admin, org.owner_id)
                .await
                .unwrap();
        } else {
            service
                .update_member_role(org.id, org.owner_id, member_id, MemberRole::Admin)
                .await
                .unwrap();
        }
        assert!(sender.permission_changes.lock().unwrap().is_empty());
        assert_eq!(*users.get_by_id_calls.lock().unwrap(), 0);
    }
}

#[tokio::test]
async fn wallet_member_requires_verified_contact_email() {
    for verified in [false, true] {
        let (mut service, sender, org, member_id, _) = service_with_update(
            MemberRole::Member,
            MemberRole::Admin,
            Vec::new(),
            false,
            Ok(EmailDeliveryOutcome::Sent { message_id: None }),
        );
        let mut user = service
            .user_repository
            .get_by_id(member_id.clone())
            .await
            .unwrap()
            .unwrap();
        user.email = "wallet.near@near".into();
        user.contact_email = Some("verified@example.com".into());
        user.contact_email_verified_at = verified.then(chrono::Utc::now);
        service.user_repository = Arc::new(StubUserRepo {
            inviter: user,
            get_by_id_calls: Mutex::new(0),
            notification_users: Default::default(),
        });
        service
            .update_member_role(org.id, org.owner_id, member_id, MemberRole::Admin)
            .await
            .unwrap();
        if verified {
            wait_for_emails(&sender, 1).await;
        }
        let emails = sender.permission_changes.lock().unwrap();
        assert_eq!(emails.len(), usize::from(verified));
        if verified {
            assert_eq!(emails[0].recipient_email, "verified@example.com");
        }
    }
}

#[tokio::test]
async fn sender_failure_does_not_fail_role_change_or_skip_demoted_owner() {
    let (service, sender, org, member_id, _) = service_with_update(
        MemberRole::Member,
        MemberRole::Owner,
        vec![OrganizationMemberRoleChange {
            user_id: UserId(Uuid::new_v4()),
            previous_role: MemberRole::Owner,
            new_role: MemberRole::Admin,
        }],
        true,
        Err(EmailError::new("provider unavailable")),
    );
    let updated = service
        .update_member_role_for_admin(org.id, member_id, MemberRole::Owner, org.owner_id)
        .await
        .unwrap();
    assert_eq!(updated.member.role, MemberRole::Owner);
    wait_for_emails(&sender, 2).await;
    assert_eq!(sender.permission_changes.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn stalled_delivery_does_not_block_role_responses_or_other_recipients() {
    for admin in [false, true] {
        let demoted_owner_id = UserId(Uuid::new_v4());
        let new_role = if admin {
            MemberRole::Owner
        } else {
            MemberRole::Admin
        };
        let additional_changes = if admin {
            vec![OrganizationMemberRoleChange {
                user_id: demoted_owner_id.clone(),
                previous_role: MemberRole::Owner,
                new_role: MemberRole::Admin,
            }]
        } else {
            Vec::new()
        };
        let (service, sender, org, member_id, _) = service_with_update(
            MemberRole::Member,
            new_role.clone(),
            additional_changes,
            admin,
            Ok(EmailDeliveryOutcome::Sent { message_id: None }),
        );
        *sender.blocked_recipient.lock().unwrap() = Some(format!("{}@example.test", member_id.0));
        tokio::time::timeout(Duration::from_secs(1), async {
            if admin {
                let update = service
                    .update_member_role_for_admin(org.id, member_id, new_role, org.owner_id)
                    .await
                    .unwrap();
                assert_eq!(update.member.role, MemberRole::Owner);
            } else {
                let update = service
                    .update_member_role(org.id, org.owner_id, member_id, new_role)
                    .await
                    .unwrap();
                assert_eq!(update.role, MemberRole::Admin);
            }
        })
        .await
        .expect("committed role changes must not wait for outbound email");
        wait_for_emails(&sender, if admin { 2 } else { 1 }).await;
        if admin {
            assert!(sender
                .permission_changes
                .lock()
                .unwrap()
                .iter()
                .any(
                    |email| email.recipient_email == format!("{}@example.test", demoted_owner_id.0)
                ));
        }
        assert!(!sender
            .blocked_delivery_cancelled
            .load(std::sync::atomic::Ordering::SeqCst));
        tokio::time::advance(MEMBER_PERMISSION_EMAIL_TIMEOUT).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            if !sender
                .blocked_delivery_cancelled
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                sender.blocked_delivery_finished.notified().await;
            }
        })
        .await
        .expect("the entire stalled delivery must be cancelled at its deadline");
    }
}
