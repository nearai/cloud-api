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
    assert!(client
        .execute("DELETE FROM organizations WHERE id = $1", &[&default])
        .await
        .is_err());
    assert!(client
        .execute(
            "UPDATE users SET default_organization_id = $1 WHERE id = $2",
            &[&other, &user.id]
        )
        .await
        .is_err());
    Ok(())
}
