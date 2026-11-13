//! Who may change an API key: its creator, or an organization owner or admin.
//! A plain member still creates keys in the shared workspace, and changes only their own.
use crate::common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

/// (case, caller, method, path, body, allowed)
type Case<'a> = (&'a str, &'a str, &'a str, String, Option<Value>, bool);

fn user_id(session: &str) -> Uuid {
    let raw = session
        .strip_prefix("rt_")
        .expect("session tokens are formatted as rt_<user uuid>");
    Uuid::parse_str(raw).expect("session token encodes a valid user UUID")
}

async fn add_member(database: &Arc<database::Database>, org_id: &str, session: &str, role: &str) {
    let org_id = Uuid::parse_str(org_id).unwrap();
    let user_id = user_id(session);
    database
        .pool()
        .get()
        .await
        .expect("database connection")
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, $3)",
            &[&org_id, &user_id, &role],
        )
        .await
        .expect("insert organization member");
}

async fn call(
    server: &axum_test::TestServer,
    session: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> axum_test::TestResponse {
    let request = match method {
        "PUT" => server.put(path),
        "PATCH" => server.patch(path),
        "DELETE" => server.delete(path),
        "POST" => server.post(path),
        "GET" => server.get(path),
        other => panic!("unsupported HTTP method in test case: {other}"),
    }
    .add_header("Authorization", format!("Bearer {session}"))
    .add_header("User-Agent", MOCK_USER_AGENT);
    match body {
        Some(body) => request.json(&body).await,
        None => request.await,
    }
}

#[tokio::test]
async fn api_key_changes_by_creator_or_admin() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner, _) = setup_unique_test_session(&database).await;
    let (member, _) = setup_unique_test_session(&database).await;
    let (admin, _) = setup_unique_test_session(&database).await;
    let org = create_org_with_session(&server, &owner).await;
    add_member(&database, &org.id, &member, "member").await;
    add_member(&database, &org.id, &admin, "admin").await;

    let workspace_id = list_workspaces_with_session(&server, org.id.clone(), &owner).await[0]
        .id
        .clone();
    let shared = format!("/v1/workspaces/{workspace_id}");
    let shared_keys = format!("{shared}/api-keys");
    let owner_key_id = create_api_key_in_workspace_with_session(
        &server,
        workspace_id.clone(),
        "owner".into(),
        &owner,
    )
    .await
    .id;
    let owner_key = format!("{shared_keys}/{owner_key_id}");

    let mut member_keys = Vec::new();
    for name in ["member-1", "member-2"] {
        let key = create_api_key_in_workspace_with_session(
            &server,
            workspace_id.clone(),
            name.into(),
            &member,
        )
        .await;
        member_keys.push(format!("{shared_keys}/{}", key.id));
    }
    let (member_key, second_member_key) = (&member_keys[0], &member_keys[1]);

    let cases: Vec<Case> = vec![
        (
            "member revokes the owner's key",
            &member,
            "DELETE",
            owner_key.clone(),
            None,
            false,
        ),
        (
            "member updates the owner's key",
            &member,
            "PATCH",
            owner_key.clone(),
            Some(json!({"is_active": false})),
            false,
        ),
        (
            "member clears the owner's key spend limit",
            &member,
            "PATCH",
            format!("{owner_key}/spend-limit"),
            Some(json!({"spendLimit": null})),
            false,
        ),
        (
            "member updates own key",
            &member,
            "PATCH",
            member_key.clone(),
            Some(json!({"name": format!("renamed-{}", Uuid::new_v4())})),
            true,
        ),
        (
            "member clears own key spend limit",
            &member,
            "PATCH",
            format!("{member_key}/spend-limit"),
            Some(json!({"spendLimit": null})),
            true,
        ),
        (
            "admin clears a member's key spend limit",
            &admin,
            "PATCH",
            format!("{member_key}/spend-limit"),
            Some(json!({"spendLimit": null})),
            true,
        ),
        (
            "owner revokes a member's key",
            &owner,
            "DELETE",
            second_member_key.clone(),
            None,
            true,
        ),
        (
            "member revokes own key",
            &member,
            "DELETE",
            member_key.clone(),
            None,
            true,
        ),
    ];
    for (case, caller, method, path, body, allowed) in cases {
        let reply = call(&server, caller, method, &path, body).await;
        if allowed {
            assert!(reply.status_code().is_success(), "{case}: {}", reply.text());
        } else {
            assert_eq!(reply.status_code(), 403, "{case}: {}", reply.text());
        }
    }

    // The refused changes left the owner's key active, and the allowed revokes took effect
    // (a revoked key is soft-deleted, so it drops out of the listing).
    let listed = call(&server, &owner, "GET", &shared_keys, None).await;
    assert_eq!(listed.status_code(), 200);
    let keys = listed.json::<Value>();
    let rows = keys["api_keys"].as_array().expect("api_keys is a list");
    let owner_row = rows
        .iter()
        .find(|k| k["id"] == owner_key_id.as_str())
        .expect("the owner's key is still listed");
    assert_eq!(owner_row["is_active"], true);
    for path in [member_key, second_member_key] {
        let revoked_id = path.rsplit('/').next().unwrap();
        assert!(
            rows.iter().all(|k| k["id"] != revoked_id),
            "{path} should be revoked"
        );
    }
}
