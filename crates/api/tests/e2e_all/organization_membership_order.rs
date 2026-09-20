//! The default is the earliest currently active membership, not the oldest org.
use crate::common::*;
use serde_json::Value;
use services::organization::{
    OrganizationOrderBy, OrganizationOrderDirection, OrganizationRepository as _,
};
use uuid::Uuid;

#[tokio::test]
async fn users_me_orders_memberships_before_pagination_and_advances_when_unavailable() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, _) = setup_unique_test_session(&database).await;
    let user = Uuid::parse_str(session.strip_prefix("rt_").unwrap()).unwrap();
    let client = database.pool().get().await.unwrap();
    let first = Uuid::new_v4();
    let tied = Uuid::new_v4();
    let first_ids = if first < tied {
        [first, tied]
    } else {
        [tied, first]
    };
    // Insert the larger UUID first to prove tie handling is independent of insertion order.
    for id in first_ids.iter().rev() {
        client
            .execute(
                "INSERT INTO organizations (id, name, created_at) VALUES ($1, $2, NOW())",
                &[id, &id.to_string()],
            )
            .await
            .unwrap();
        client.execute("INSERT INTO organization_members (organization_id, user_id, role, joined_at) VALUES ($1, $2, 'owner', '2026-01-01')", &[id, &user]).await.unwrap();
    }
    // These older organizations were joined later. The first membership must
    // still appear on the first page even with >100 organizations in the account.
    for _ in 0..101 {
        let id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO organizations (id, name, created_at) VALUES ($1, $2, '2025-01-01')",
                &[&id, &id.to_string()],
            )
            .await
            .unwrap();
        client.execute("INSERT INTO organization_members (organization_id, user_id, role, joined_at) VALUES ($1, $2, 'owner', '2026-02-01')", &[&id, &user]).await.unwrap();
    }
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    response.assert_status_ok();
    let me = response.json::<Value>();
    let orgs = me["organizations"].as_array().unwrap();
    assert_eq!(orgs.len(), 100);
    assert_eq!(orgs[0]["id"], first_ids[0].to_string());
    assert_eq!(orgs[1]["id"], first_ids[1].to_string());
    // VPC and staking use the non-role repository path; cover both SQL queries.
    let repository = database::repositories::PgOrganizationRepository::new(database.pool().clone());
    let defaults = repository
        .list_organizations_by_user(
            user,
            1,
            0,
            Some(OrganizationOrderBy::JoinedAt),
            Some(OrganizationOrderDirection::Asc),
        )
        .await
        .unwrap();
    assert_eq!(defaults[0].id.0, first_ids[0]);
    // The public listing supports explicit membership order as well.
    let response = server
        .get("/v1/organizations?order_by=joined_at&order_direction=asc&limit=1")
        .add_header("Authorization", format!("Bearer {session}"))
        .await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["organizations"][0]["id"], first_ids[0].to_string());
    // Default /organizations ordering remains organization creation time.
    let page = server
        .get("/v1/organizations?limit=1")
        .add_header("Authorization", format!("Bearer {session}"))
        .await
        .json::<Value>();
    assert!(!first_ids
        .iter()
        .any(|id| page["organizations"][0]["id"] == id.to_string()));
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&first_ids[0]],
        )
        .await
        .unwrap();
    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .await
        .json::<Value>();
    assert_eq!(me["organizations"][0]["id"], first_ids[1].to_string());
    client
        .execute(
            "DELETE FROM organization_members WHERE user_id = $1 AND organization_id = $2",
            &[&user, &first_ids[1]],
        )
        .await
        .unwrap();
    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .await
        .json::<Value>();
    assert!(me["organizations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|org| !first_ids.iter().any(|id| org["id"] == id.to_string())));
}
