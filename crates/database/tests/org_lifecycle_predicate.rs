#[allow(dead_code)]
mod support;

use database::repositories::lifecycle::org_lifecycle_predicate;
use services::admin::OrganizationLifecycleFilter as F;
use support::test_pool;
use uuid::Uuid;

#[tokio::test]
async fn predicate_agrees_with_derivation_for_an_all_erased_org() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let client = pool.get().await?;
    let user_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    let suffix = org_id.simple().to_string();

    client
        .execute(
            "INSERT INTO users (id, email, username, created_at, updated_at, is_active, auth_provider, provider_user_id)
             VALUES ($1, $2, $3, now(), now(), false, 'erased', $4)",
            &[
                &user_id,
                &format!("lifecycle-{suffix}@example.test"),
                &format!("lifecycle-{suffix}"),
                &format!("erased-{suffix}"),
            ],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
             VALUES ($1, $2, NULL, now(), now(), false)",
            &[&org_id, &format!("lifecycle-org-{suffix}")],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_members (id, organization_id, user_id, role, joined_at)
             VALUES ($1, $2, $3, 'owner', now())",
            &[&Uuid::new_v4(), &org_id, &user_id],
        )
        .await?;

    for (filter, expected) in [(F::Erased, 1), (F::Deleted, 0), (F::Active, 0), (F::All, 1)] {
        let sql = format!(
            "SELECT o.id FROM organizations o WHERE o.id = $1 AND {}",
            org_lifecycle_predicate(filter)
        );
        let rows = client.query(sql.as_str(), &[&org_id]).await?;
        assert_eq!(rows.len(), expected, "{filter:?}");
    }

    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id = $1",
            &[&org_id],
        )
        .await?;
    client
        .execute("DELETE FROM organizations WHERE id = $1", &[&org_id])
        .await?;
    client
        .execute("DELETE FROM users WHERE id = $1", &[&user_id])
        .await?;
    Ok(())
}
