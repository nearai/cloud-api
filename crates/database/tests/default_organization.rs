// This test uses the shared pool but not the usage-reporting fixtures.
#[allow(dead_code)]
mod support;

use database::repositories::{PgOrganizationRepository, UserRepository};
use services::organization::{DeleteOrganizationResult, OrganizationRepository as _};
use uuid::Uuid;

#[tokio::test]
async fn default_survives_older_joins_pagination_and_membership_changes() -> anyhow::Result<()> {
    let pool = support::test_pool().await?;
    let client = pool.get().await?;
    let users = UserRepository::new(pool.clone());
    let orgs = PgOrganizationRepository::new(pool.clone());
    let suffix = Uuid::new_v4().to_string();
    let user = users
        .create_from_oauth(
            format!("{suffix}@example.test"),
            suffix.clone(),
            None,
            None,
            "test".into(),
            suffix,
        )
        .await?;
    let default = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO organizations (id, name) VALUES ($1, $2)",
            &[&default, &format!("default-{default}")],
        )
        .await?;
    client.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", &[&default, &user.id]).await?;
    let mut other = default;
    for _ in 0..101 {
        other = Uuid::new_v4();
        client.execute("INSERT INTO organizations (id, name, created_at) VALUES ($1, $2, NOW() - INTERVAL '1 year')", &[&other, &format!("older-{other}")]).await?;
        client.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", &[&other, &user.id]).await?;
    }
    let page = orgs
        .list_organizations_with_roles_by_user(user.id, 100, 0, None, None)
        .await?;
    assert_eq!(page.len(), 100);
    assert!(!page.iter().any(|org| org.organization.id.0 == default));
    assert_eq!(
        users
            .get_by_id(user.id)
            .await?
            .unwrap()
            .default_organization_id,
        Some(default)
    );
    assert_eq!(
        orgs.delete_if_no_staking_farm_source(default, user.id)
            .await?,
        DeleteOrganizationResult::DefaultOrganization
    );
    assert_eq!(
        orgs.delete_if_no_staking_farm_source(other, user.id)
            .await?,
        DeleteOrganizationResult::Deleted
    );
    // Membership deletion and ownership changes must not free the default for deletion.
    client.execute("UPDATE organization_members SET role = 'member' WHERE organization_id = $1 AND user_id = $2", &[&default, &user.id]).await?;
    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&default, &user.id],
        )
        .await?;
    assert_eq!(
        users
            .get_by_id(user.id)
            .await?
            .unwrap()
            .default_organization_id,
        Some(default)
    );
    let error = client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&default],
        )
        .await
        .unwrap_err();
    assert!(error
        .as_db_error()
        .unwrap()
        .message()
        .contains("Default organizations"));
    let error = client
        .execute("DELETE FROM organizations WHERE id = $1", &[&default])
        .await
        .unwrap_err();
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Default organizations cannot be deleted or deactivated"
    );
    assert!(client
        .execute(
            "UPDATE users SET default_organization_id = $1 WHERE id = $2",
            &[&other, &user.id]
        )
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn concurrent_first_memberships_follow_serialized_join_order() -> anyhow::Result<()> {
    use std::time::Duration;
    let pool = support::test_pool().await?;
    let users = UserRepository::new(pool.clone());
    let suffix = Uuid::new_v4().to_string();
    let user = users
        .create_from_oauth(
            format!("{suffix}@example.test"),
            suffix.clone(),
            None,
            None,
            "test".into(),
            suffix,
        )
        .await?;
    let mut first_client = pool.get().await?;
    let mut second_client = pool.get().await?;
    let first_org = Uuid::new_v4();
    let second_org = Uuid::new_v4();
    for id in [first_org, second_org] {
        first_client
            .execute(
                "INSERT INTO organizations (id, name) VALUES ($1, $2)",
                &[&id, &id.to_string()],
            )
            .await?;
    }
    // Begin the second join's transaction first. NOW() would incorrectly give
    // that membership an earlier timestamp, despite inserting after the first.
    let second = second_client.transaction().await?;
    second.query_one("SELECT NOW()", &[]).await?;
    let first = first_client.transaction().await?;
    first.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", &[&first_org, &user.id]).await?;
    {
        let second_params: &[&(dyn tokio_postgres::types::ToSql + Sync)] = &[&second_org, &user.id];
        let insert = second.execute("INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')", second_params);
        tokio::pin!(insert);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut insert)
                .await
                .is_err(),
            "second join must wait for the first transaction"
        );
        first.commit().await?;
        tokio::time::timeout(Duration::from_secs(5), &mut insert).await??;
    }
    second.commit().await?;
    let row = first_client.query_one("SELECT default_organization_id, (SELECT organization_id FROM organization_members WHERE user_id = users.id ORDER BY joined_at, organization_id LIMIT 1) AS earliest FROM users WHERE id = $1", &[&user.id]).await?;
    assert_eq!(row.get::<_, Uuid>("default_organization_id"), first_org);
    assert_eq!(row.get::<_, Uuid>("earliest"), first_org);
    Ok(())
}

#[tokio::test]
async fn registration_commits_complete_signup_or_rolls_it_all_back() -> anyhow::Result<()> {
    use services::auth::{DefaultOrganizationSource, OAuthUserInfo, UserRepository as _};
    let pool = support::test_pool().await?;
    let users = UserRepository::new(pool.clone());
    let client = pool.get().await?;
    let suffix = Uuid::new_v4().to_string();
    let info = OAuthUserInfo {
        provider: "test".into(),
        provider_user_id: suffix.clone(),
        email: format!("{suffix}@example.test"),
        username: suffix,
        display_name: None,
        avatar_url: None,
    };
    let user = users.register_from_oauth(info.clone()).await?;
    assert_eq!(
        user.default_organization_source,
        DefaultOrganizationSource::FirstMembership
    );
    let org = user.default_organization_id.unwrap();
    assert_eq!(client.query_one("SELECT COUNT(*) FROM workspaces WHERE organization_id = $1 AND created_by_user_id = $2", &[&org, &user.id.0]).await?.get::<_, i64>(0), 1);
    // Force failure at workspace insertion too, after user and membership writes.
    let mut failed_info = info;
    failed_info.provider_user_id = Uuid::new_v4().to_string();
    failed_info.email = format!("{}@example.test", failed_info.provider_user_id);
    let trigger_name = format!("reject_signup_{}", Uuid::new_v4().simple());
    client.batch_execute(&format!("CREATE FUNCTION {trigger_name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS (SELECT 1 FROM users WHERE id = NEW.created_by_user_id AND provider_user_id = '{}') THEN RAISE EXCEPTION 'test workspace failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER {trigger_name} BEFORE INSERT ON workspaces FOR EACH ROW EXECUTE FUNCTION {trigger_name}();", failed_info.provider_user_id)).await?;
    let result = users.register_from_oauth(failed_info.clone()).await;
    client
        .batch_execute(&format!(
            "DROP TRIGGER {trigger_name} ON workspaces; DROP FUNCTION {trigger_name}();"
        ))
        .await?;
    assert!(result.is_err());
    assert_eq!(
        client
            .query_one(
                "SELECT COUNT(*) FROM users WHERE provider_user_id = $1",
                &[&failed_info.provider_user_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT COUNT(*) FROM organizations WHERE name LIKE $1",
                &[&format!("{}-org-%", failed_info.provider_user_id)]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    Ok(())
}
