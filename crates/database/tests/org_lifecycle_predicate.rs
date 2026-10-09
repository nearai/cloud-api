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

    let mut failures: Vec<String> = Vec::new();
    for (filter, expected) in [(F::Erased, 1), (F::Deleted, 0), (F::Active, 0), (F::All, 1)] {
        let sql = format!(
            "SELECT o.id FROM organizations o WHERE o.id = $1 AND {}",
            org_lifecycle_predicate(filter)
        );
        match client.query(sql.as_str(), &[&org_id]).await {
            Ok(rows) if rows.len() == expected => {}
            Ok(rows) => failures.push(format!(
                "{filter:?}: expected {expected}, got {}",
                rows.len()
            )),
            Err(e) => failures.push(format!("{filter:?}: query error: {e}")),
        }
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
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}

#[tokio::test]
async fn predicate_classifies_deleted_and_active_orgs() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let client = pool.get().await?;
    let erased_user = Uuid::new_v4();
    let live_user = Uuid::new_v4();
    let mixed_org = Uuid::new_v4(); // inactive, one erased + one live member
    let active_org = Uuid::new_v4(); // active, one live member
    let empty_org = Uuid::new_v4(); // inactive, no members
    let suffix = Uuid::new_v4().simple().to_string();

    for (id, provider, active) in [(erased_user, "erased", false), (live_user, "github", true)] {
        client
            .execute(
                "INSERT INTO users (id, email, username, created_at, updated_at, is_active, auth_provider, provider_user_id)
                 VALUES ($1, $2, $3, now(), now(), $4, $5, $6)",
                &[
                    &id,
                    &format!("lifecycle-{id}@example.test"),
                    &format!("lifecycle-{id}"),
                    &active,
                    &provider,
                    &format!("{provider}-{id}"),
                ],
            )
            .await?;
    }
    for (id, active) in [(mixed_org, false), (active_org, true), (empty_org, false)] {
        client
            .execute(
                "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
                 VALUES ($1, $2, NULL, now(), now(), $3)",
                &[&id, &format!("lifecycle-{id}-{suffix}"), &active],
            )
            .await?;
    }
    for (org, user) in [
        (mixed_org, erased_user),
        (mixed_org, live_user),
        (active_org, live_user),
    ] {
        client
            .execute(
                "INSERT INTO organization_members (id, organization_id, user_id, role, joined_at)
                 VALUES ($1, $2, $3, 'member', now())",
                &[&Uuid::new_v4(), &org, &user],
            )
            .await?;
    }

    // (org, [active, deleted, erased, all] matches)
    let cases = [
        (mixed_org, [false, true, false, true]),
        (active_org, [true, false, false, true]),
        (empty_org, [false, true, false, true]),
    ];
    let mut failures = Vec::new();
    for (org, expected) in cases {
        for (filter, want) in [F::Active, F::Deleted, F::Erased, F::All]
            .into_iter()
            .zip(expected)
        {
            let sql = format!(
                "SELECT o.id FROM organizations o WHERE o.id = $1 AND {}",
                org_lifecycle_predicate(filter)
            );
            let got = !client.query(sql.as_str(), &[&org]).await?.is_empty();
            if got != want {
                failures.push(format!("{org} {filter:?}: got {got}, want {want}"));
            }
        }
    }

    // Clean up before asserting so a failure does not leak rows.
    let orgs = vec![mixed_org, active_org, empty_org];
    let users = vec![erased_user, live_user];
    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id = ANY($1)",
            &[&orgs],
        )
        .await?;
    client
        .execute("DELETE FROM organizations WHERE id = ANY($1)", &[&orgs])
        .await?;
    client
        .execute("DELETE FROM users WHERE id = ANY($1)", &[&users])
        .await?;
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}
