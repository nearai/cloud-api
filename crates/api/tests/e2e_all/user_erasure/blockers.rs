use super::*;

#[tokio::test]
async fn preview_lists_personal_org_as_erased_and_writes_nothing() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, _email) = new_user(&database).await;
    let org_id = personal_org_id(&server, &session).await;

    let response = preview(&server, user_id).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["lifecycle"], "active");
    assert_eq!(body["blockers"].as_array().unwrap().len(), 0);
    assert_eq!(body["erased_organizations"][0]["id"], org_id);
    assert_eq!(body["erased_organizations"][0]["workspaces"], 1);
    assert!(body["log"].is_null());

    let client = database.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT is_active, auth_provider FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>("is_active"), "preview must not write");
    assert_eq!(row.get::<_, String>("auth_provider"), "github");
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM user_erasure_log WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn preview_unknown_user_is_404() {
    let (server, _database) = setup_test_server_with_database().await;
    assert_eq!(
        preview(&server, uuid::Uuid::new_v4()).await.status_code(),
        404
    );
}

#[tokio::test]
async fn preview_blocks_sole_owner_of_active_shared_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert_eq!(blocker_codes(&body), vec!["sole_owner_of_shared_org"]);
    assert_eq!(body["blockers"][0]["organization_id"], team.id);
}

#[tokio::test]
async fn preview_allows_owner_after_ownership_transfer() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    transfer_ownership(&server, &team.id, teammate).await;

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert!(blocker_codes(&body).is_empty(), "{body}");
    assert!(body["retained_organizations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == team.id));
}

#[tokio::test]
async fn preview_does_not_block_on_owner_deleted_shared_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&team_uuid],
        )
        .await
        .unwrap();

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert!(blocker_codes(&body).is_empty(), "{body}");
}

#[tokio::test]
async fn preview_blocks_active_or_stale_staking() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, _) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_staking_source(&client, org, user_id, r#"[{"amount":"10"}]"#, 1).await;
    assert_eq!(
        blocker_codes(&preview(&server, user_id).await.json()),
        vec!["staking_active"]
    );

    client
        .execute(
            "UPDATE organization_staking_farm_sources SET active_positions = '[]'::jsonb, \
             last_synced_at = NOW() - INTERVAL '48 hours' WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert_eq!(
        blocker_codes(&preview(&server, user_id).await.json()),
        vec!["staking_active"],
        "stale sync must still block"
    );

    client
        .execute(
            "UPDATE organization_staking_farm_sources SET last_synced_at = NOW() \
             WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert!(blocker_codes(&preview(&server, user_id).await.json()).is_empty());
}

#[tokio::test]
async fn preview_blocks_active_admin_tokens_only() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, _) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    insert_admin_token(&client, user_id, false).await;
    assert!(
        blocker_codes(&preview(&server, user_id).await.json()).is_empty(),
        "revoked tokens do not block"
    );
    insert_admin_token(&client, user_id, true).await;
    let body = preview(&server, user_id).await.json::<serde_json::Value>();
    assert_eq!(blocker_codes(&body), vec!["active_admin_tokens"]);
    assert_eq!(body["blockers"][0]["count"], 1);
}

#[tokio::test]
async fn erase_blocked_returns_structured_blockers_and_changes_nothing() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;

    let response = erase(&server, owner, &email).await;
    assert_eq!(response.status_code(), 409, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["blockers"][0]["code"], "sole_owner_of_shared_org");
    let row = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT is_active, email FROM users WHERE id = $1",
            &[&owner],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>("is_active"));
    assert_eq!(row.get::<_, String>("email"), email);
}

#[tokio::test]
async fn erase_blocked_by_active_admin_token() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, email) = new_user(&database).await;
    insert_admin_token(&database.pool().get().await.unwrap(), user_id, true).await;
    let r = erase(&server, user_id, &email).await;
    assert_eq!(r.status_code(), 409, "{}", r.text());
    assert_eq!(
        r.json::<serde_json::Value>()["blockers"][0]["code"],
        "active_admin_tokens"
    );
}

#[tokio::test]
async fn erase_rejects_admin_access_token() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let created = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": format!("erasure-{}", uuid::Uuid::new_v4()),
            "reason": "erasure access token test",
            "expires_in_hours": 1
        }))
        .await;
    assert_eq!(created.status_code(), 200, "{}", created.text());
    let token = created.json::<serde_json::Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let preview = server
        .post(format!("/v1/admin/users/{user_id}/erasure/preview").as_str())
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(preview.status_code(), 200, "{}", preview.text());

    let execute = server
        .post(format!("/v1/admin/users/{user_id}/erasure").as_str())
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "confirm_email": email }))
        .await;
    assert!(
        matches!(execute.status_code().as_u16(), 401 | 403),
        "{} {}",
        execute.status_code(),
        execute.text()
    );
    let active: bool = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one("SELECT is_active FROM users WHERE id = $1", &[&user_id])
        .await
        .unwrap()
        .get(0);
    assert!(active, "a rejected erase must leave the user active");
}

#[tokio::test]
async fn preview_blocks_staking_source_created_by_user_in_retained_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    transfer_ownership(&server, &team.id, teammate).await;
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_staking_source(&client, team_uuid, owner, r#"[{"amount":"10"}]"#, 1).await;

    let body = preview(&server, owner).await.json::<serde_json::Value>();
    assert_eq!(blocker_codes(&body), vec!["staking_active"], "{body}");
}
