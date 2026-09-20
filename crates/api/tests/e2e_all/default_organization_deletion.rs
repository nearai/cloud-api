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
