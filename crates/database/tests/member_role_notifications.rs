#[allow(dead_code)]
mod support;

#[path = "member_role_notifications/commit_reply_proxy.rs"]
mod commit_reply_proxy;

use database::{DbPool, PgOrganizationRepository};
use services::organization::ports::{
    MemberRole, OrganizationRepository, UpdateOrganizationMemberRequest,
};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn lost_commit_reply_preserves_both_role_change_paths() -> anyhow::Result<()> {
    for admin_role in [None, Some(MemberRole::Admin), Some(MemberRole::Owner)] {
        let new_role = admin_role.clone().unwrap_or(MemberRole::Admin);
        let transfers_ownership = new_role == MemberRole::Owner;
        let pool = support::test_pool().await?;
        let (org_id, owner_id, member_id) = fixture(&pool).await?;
        let proxy = commit_reply_proxy::CommitReplyProxy::start(false).await?;
        let repository = PgOrganizationRepository::new(proxy.pool.clone());
        let update = tokio::time::timeout(Duration::from_secs(10), async {
            if admin_role.is_some() {
                repository
                    .update_member_role_with_audit(
                        org_id,
                        member_id,
                        UpdateOrganizationMemberRequest {
                            role: new_role.clone(),
                        },
                        owner_id,
                    )
                    .await
            } else {
                repository
                    .update_member(
                        org_id,
                        member_id,
                        UpdateOrganizationMemberRequest {
                            role: MemberRole::Admin,
                        },
                        owner_id,
                    )
                    .await
            }
        })
        .await??;
        assert!(proxy.intercepted.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(update.previous_role, MemberRole::Member);
        assert_eq!(
            update.member.role,
            if transfers_ownership {
                MemberRole::Owner
            } else {
                MemberRole::Admin
            }
        );
        if transfers_ownership {
            assert_eq!(update.additional_changes.len(), 1);
            assert_eq!(update.additional_changes[0].user_id.0, owner_id);
            assert_eq!(
                update.additional_changes[0].previous_role,
                MemberRole::Owner
            );
            assert_eq!(update.additional_changes[0].new_role, MemberRole::Admin);
        } else {
            assert!(update.additional_changes.is_empty());
        }
        let client = pool.get().await?;
        let audit_count: i64 = client.query_one("SELECT COUNT(*) FROM organization_member_role_audit_log WHERE organization_id = $1", &[&org_id]).await?.get(0);
        assert_eq!(audit_count, if transfers_ownership { 2 } else { 1 });
    }
    Ok(())
}

#[tokio::test]
async fn lost_rollback_reply_does_not_report_a_successful_role_change() -> anyhow::Result<()> {
    for admin_role in [None, Some(MemberRole::Admin), Some(MemberRole::Owner)] {
        let pool = support::test_pool().await?;
        let (org_id, owner_id, member_id) = fixture(&pool).await?;
        let proxy = commit_reply_proxy::CommitReplyProxy::start(true).await?;
        let repository = PgOrganizationRepository::new(proxy.pool.clone());
        let update = tokio::time::timeout(Duration::from_secs(10), async {
            if let Some(role) = admin_role.clone() {
                repository
                    .update_member_role_with_audit(
                        org_id,
                        member_id,
                        UpdateOrganizationMemberRequest { role },
                        owner_id,
                    )
                    .await
            } else {
                repository
                    .update_member(
                        org_id,
                        member_id,
                        UpdateOrganizationMemberRequest {
                            role: MemberRole::Admin,
                        },
                        owner_id,
                    )
                    .await
            }
        })
        .await?;
        assert!(proxy.intercepted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(update.is_err());
        let client = pool.get().await?;
        let role: String = client
            .query_one(
                "SELECT role FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                &[&org_id, &member_id],
            )
            .await?
            .get(0);
        assert_eq!(role, "member");
        let audit_count: i64 = client.query_one("SELECT COUNT(*) FROM organization_member_role_audit_log WHERE organization_id = $1", &[&org_id]).await?.get(0);
        assert_eq!(audit_count, 0);
    }
    Ok(())
}

async fn fixture(pool: &DbPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let client = pool.get().await?;
    let org_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let member_id = Uuid::new_v4();
    for user_id in [owner_id, member_id] {
        client.execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id) VALUES ($1, $2, $2, 'test', $2)",
            &[&user_id, &format!("role-email-{user_id}@example.test")],
        ).await?;
    }
    client
        .execute(
            "INSERT INTO organizations (id, name) VALUES ($1, $2)",
            &[&org_id, &format!("role-email-{org_id}")],
        )
        .await?;
    for (user_id, role) in [(owner_id, "owner"), (member_id, "member")] {
        client.execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, $3)",
            &[&org_id, &user_id, &role],
        ).await?;
    }
    Ok((org_id, owner_id, member_id))
}

#[tokio::test]
async fn concurrent_update_returns_role_from_locked_transaction() -> anyhow::Result<()> {
    let pool = support::test_pool().await?;
    let (org_id, owner_id, member_id) = fixture(&pool).await?;
    let mut first = pool.get().await?;
    let transaction = first.transaction().await?;
    transaction
        .query_one(
            "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
            &[&org_id],
        )
        .await?;
    transaction.execute(
        "UPDATE organization_members SET role = 'admin' WHERE organization_id = $1 AND user_id = $2",
        &[&org_id, &member_id],
    ).await?;
    let blocker: i32 = transaction
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get(0);
    let repository = PgOrganizationRepository::new(pool.clone());
    let second = tokio::spawn(async move {
        repository
            .update_member(
                org_id,
                member_id,
                UpdateOrganizationMemberRequest {
                    role: MemberRole::Member,
                },
                owner_id,
            )
            .await
    });
    let observer = pool.get().await?;
    // Prove the second operation is waiting on the first before committing it.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = observer.query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
                &[&blocker],
            ).await?.get(0);
            if waiting {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await??;
    transaction.commit().await?;
    let update = tokio::time::timeout(Duration::from_secs(10), second).await???;
    assert_eq!(update.previous_role, MemberRole::Admin);
    assert_eq!(update.member.role, MemberRole::Member);
    assert!(update.additional_changes.is_empty());
    Ok(())
}

#[tokio::test]
async fn audited_transfer_returns_every_role_change_and_noop_returns_none() -> anyhow::Result<()> {
    let pool = support::test_pool().await?;
    let (org_id, owner_id, member_id) = fixture(&pool).await?;
    let repository = PgOrganizationRepository::new(pool.clone());
    let update = repository
        .update_member_role_with_audit(
            org_id,
            member_id,
            UpdateOrganizationMemberRequest {
                role: MemberRole::Owner,
            },
            owner_id,
        )
        .await?;
    assert_eq!(update.previous_role, MemberRole::Member);
    assert_eq!(update.member.role, MemberRole::Owner);
    assert_eq!(update.additional_changes.len(), 1);
    let demoted = &update.additional_changes[0];
    assert_eq!(demoted.user_id.0, owner_id);
    assert_eq!(demoted.previous_role, MemberRole::Owner);
    assert_eq!(demoted.new_role, MemberRole::Admin);
    let client = pool.get().await?;
    let audit = client.query(
        "SELECT member_user_id, previous_role, new_role FROM organization_member_role_audit_log WHERE organization_id = $1",
        &[&org_id],
    ).await?;
    assert_eq!(audit.len(), 2);
    assert!(audit
        .iter()
        .any(|row| row.get::<_, Uuid>("member_user_id") == owner_id
            && row.get::<_, String>("previous_role") == "owner"
            && row.get::<_, String>("new_role") == "admin"));
    assert!(audit
        .iter()
        .any(|row| row.get::<_, Uuid>("member_user_id") == member_id
            && row.get::<_, String>("previous_role") == "member"
            && row.get::<_, String>("new_role") == "owner"));
    let unchanged = repository
        .update_member_role_with_audit(
            org_id,
            member_id,
            UpdateOrganizationMemberRequest {
                role: MemberRole::Owner,
            },
            owner_id,
        )
        .await?;
    assert_eq!(unchanged.previous_role, unchanged.member.role);
    assert!(unchanged.additional_changes.is_empty());
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_member_role_audit_log WHERE organization_id = $1",
            &[&org_id],
        )
        .await?
        .get(0);
    assert_eq!(count, 2);
    Ok(())
}
