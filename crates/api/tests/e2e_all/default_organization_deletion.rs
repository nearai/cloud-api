use crate::common::*;
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
async fn deletion_protects_every_members_first_active_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _) = setup_unique_test_session(&database).await;
    let owner = Uuid::parse_str(owner_session.strip_prefix("rt_").unwrap()).unwrap();
    let default = create_org_with_session(&server, &owner_session).await;
    let shared = create_org_with_session(&server, &owner_session).await;
    let default_id = Uuid::parse_str(&default.id).unwrap();
    let shared_id = Uuid::parse_str(&shared.id).unwrap();
    let client = database.pool().get().await.unwrap();
    // Backdating organization creation cannot make a later join the default.
    client
        .execute(
            "UPDATE organizations SET created_at = '2000-01-01' WHERE id = $1",
            &[&shared_id],
        )
        .await
        .unwrap();
    // Exercise >100 memberships: the guard must not depend on a listing page.
    for _ in 0..101 {
        let id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO organizations (id, name, created_at) VALUES ($1, $2, '2000-01-01')",
                &[&id, &id.to_string()],
            )
            .await
            .unwrap();
        client.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", &[&id, &owner]).await.unwrap();
    }
    let response = server
        .delete(&format!("/v1/organizations/{}", default.id))
        .add_header("Authorization", format!("Bearer {owner_session}"))
        .await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "default_organization"
    );
    let active: bool = client
        .query_one(
            "SELECT is_active FROM organizations WHERE id = $1",
            &[&default_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(active);
    // Although shared is not the owner's default, it is this member's first.
    let (member_session, _) = setup_unique_test_session(&database).await;
    let member = Uuid::parse_str(member_session.strip_prefix("rt_").unwrap()).unwrap();
    client.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'member')", &[&shared_id, &member]).await.unwrap();
    let response = server
        .delete(&format!("/v1/organizations/{}", shared.id))
        .add_header("Authorization", format!("Bearer {member_session}"))
        .await;
    assert_eq!(response.status_code(), 403);
    let response = server
        .delete(&format!("/v1/organizations/{}", shared.id))
        .add_header("Authorization", format!("Bearer {owner_session}"))
        .await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "default_organization"
    );
    // Dynamic semantics: when that membership leaves, shared is nobody's default.
    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&shared_id, &member],
        )
        .await
        .unwrap();
    let response = server
        .delete(&format!("/v1/organizations/{}", shared.id))
        .add_header("Authorization", format!("Bearer {owner_session}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
}

#[tokio::test]
async fn deletion_uses_uuid_ties_and_ignores_inactive_earlier_memberships() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, _) = setup_unique_test_session(&database).await;
    let user = Uuid::parse_str(session.strip_prefix("rt_").unwrap()).unwrap();
    let a = create_org_with_session(&server, &session).await;
    let b = create_org_with_session(&server, &session).await;
    let mut ids = [
        Uuid::parse_str(&a.id).unwrap(),
        Uuid::parse_str(&b.id).unwrap(),
    ];
    ids.sort();
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organization_members SET joined_at = '2026-01-01' WHERE user_id = $1",
            &[&user],
        )
        .await
        .unwrap();
    let response = server
        .delete(&format!("/v1/organizations/{}", ids[0]))
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
    let response = server
        .delete(&format!("/v1/organizations/{}", ids[1]))
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    // Model historical inactive data. The remaining active membership is protected.
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&ids[0]],
        )
        .await
        .unwrap();
    let next = create_org_with_session(&server, &session).await;
    let response = server
        .delete(&format!("/v1/organizations/{}", next.id))
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
}

#[tokio::test]
async fn invitation_acceptance_rejects_deleted_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _) = setup_unique_test_session(&database).await;
    let _default = create_org_with_session(&server, &owner_session).await;
    let org = create_org_with_session(&server, &owner_session).await;
    let org_id = Uuid::parse_str(&org.id).unwrap();
    let owner = Uuid::parse_str(owner_session.strip_prefix("rt_").unwrap()).unwrap();
    let (member_session, _) = setup_unique_test_session(&database).await;
    // MockAuthService uses this email for authenticated users.
    let email = "admin@test.com";
    let member = Uuid::parse_str(member_session.strip_prefix("rt_").unwrap()).unwrap();
    let invitation_id = Uuid::new_v4();
    let token = Uuid::new_v4().to_string();
    let client = database.pool().get().await.unwrap();
    client.execute(
        "INSERT INTO organization_invitations (id, organization_id, email, role, invited_by_user_id, status, token, expires_at)
         VALUES ($1, $2, $3, 'member', $4, 'pending', $5, NOW() + INTERVAL '7 days')",
        &[&invitation_id, &org_id, &email, &owner, &token],
    ).await.unwrap();
    let deleted = server
        .delete(&format!("/v1/organizations/{org_id}"))
        .add_header("Authorization", format!("Bearer {owner_session}"))
        .await;
    assert_eq!(deleted.status_code(), 200, "{}", deleted.text());
    for path in [
        format!("/v1/users/me/invitations/{invitation_id}/accept"),
        format!("/v1/invitations/{token}/accept"),
    ] {
        let response = server
            .post(&path)
            .add_header("Authorization", format!("Bearer {member_session}"))
            .await;
        assert_eq!(response.status_code(), 404, "{}", response.text());
    }
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&org_id, &member],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    let status: String = client
        .query_one(
            "SELECT status FROM organization_invitations WHERE id = $1",
            &[&invitation_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn membership_creation_serializes_with_deletion() {
    use services::common::RepositoryError;
    use services::organization::{
        AddOrganizationMemberRequest, MemberRole, OrganizationRepository,
    };
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _) = setup_unique_test_session(&database).await;
    let _default = create_org_with_session(&server, &owner_session).await;
    let owner = Uuid::parse_str(owner_session.strip_prefix("rt_").unwrap()).unwrap();
    let (member_session, _) = setup_unique_test_session(&database).await;
    let member = Uuid::parse_str(member_session.strip_prefix("rt_").unwrap()).unwrap();
    // A rollback must allow the waiting insertion; a committed deletion must reject it.
    for delete in [false, true] {
        let org = create_org_with_session(&server, &owner_session).await;
        let org_id = Uuid::parse_str(&org.id).unwrap();
        let mut client = database.pool().get().await.unwrap();
        let tx = client.transaction().await.unwrap();
        let pid: i32 = tx
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        tx.query_one(
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&org_id],
        )
        .await
        .unwrap();
        let pool = database.pool().clone();
        let task = tokio::spawn(async move {
            database::repositories::PgOrganizationRepository::new(pool)
                .add_member(
                    org_id,
                    AddOrganizationMemberRequest {
                        user_id: member,
                        role: MemberRole::Member,
                    },
                    owner,
                )
                .await
        });
        let observer = database.pool().get().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: bool = observer.query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))", &[&pid],
                ).await.unwrap().get(0);
                if blocked { break; }
                assert!(!task.is_finished(), "membership creation must wait for the parent lock");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("membership insertion should block");
        if delete {
            tx.execute(
                "UPDATE organizations SET is_active = false WHERE id = $1",
                &[&org_id],
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        if delete {
            assert!(matches!(result, Err(RepositoryError::NotFound(_))));
        } else {
            result.unwrap();
            // A committed first membership is now visible to the real deletion guard.
            let response = server
                .delete(&format!("/v1/organizations/{org_id}"))
                .add_header("Authorization", format!("Bearer {owner_session}"))
                .await;
            assert_eq!(response.status_code(), 409, "{}", response.text());
            observer
                .execute(
                    "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                    &[&org_id, &member],
                )
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn deletion_waits_for_new_default_membership() {
    use services::organization::{
        AddOrganizationMemberRequest, DeleteOrganizationResult, MemberRole, OrganizationRepository,
    };
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _) = setup_unique_test_session(&database).await;
    let _default = create_org_with_session(&server, &owner_session).await;
    let owner = Uuid::parse_str(owner_session.strip_prefix("rt_").unwrap()).unwrap();
    let org = create_org_with_session(&server, &owner_session).await;
    let org_id = Uuid::parse_str(&org.id).unwrap();
    let (member_session, _) = setup_unique_test_session(&database).await;
    let member = Uuid::parse_str(member_session.strip_prefix("rt_").unwrap()).unwrap();
    let mut client = database.pool().get().await.unwrap();
    let tx = client.transaction().await.unwrap();
    let pid: i32 = tx
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    // Pause the real membership insert at its user foreign-key check, after it
    // has locked the organization. This makes the member-first ordering deterministic.
    tx.query_one("SELECT id FROM users WHERE id = $1 FOR UPDATE", &[&member])
        .await
        .unwrap();
    let pool = database.pool().clone();
    let member_task = tokio::spawn(async move {
        database::repositories::PgOrganizationRepository::new(pool)
            .add_member(
                org_id,
                AddOrganizationMemberRequest {
                    user_id: member,
                    role: MemberRole::Member,
                },
                owner,
            )
            .await
    });
    let observer = database.pool().get().await.unwrap();
    let member_pid: i32 = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(row) = observer
                .query_opt(
                    "SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                    &[&pid],
                )
                .await
                .unwrap()
            {
                break row.get(0);
            }
            assert!(!member_task.is_finished());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("membership insert should reach its foreign-key check");
    let pool = database.pool().clone();
    let delete_task = tokio::spawn(async move {
        database::repositories::PgOrganizationRepository::new(pool)
            .delete_if_no_staking_farm_source(org_id, owner)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked: bool = observer.query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))", &[&member_pid],
            ).await.unwrap().get(0);
            if blocked { break; }
            assert!(!delete_task.is_finished(), "deletion must wait for membership creation");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("deletion should block on the membership transaction");
    tx.rollback().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), member_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), delete_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        DeleteOrganizationResult::DefaultOrganization
    ));
}
