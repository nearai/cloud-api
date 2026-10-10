use super::*;

#[tokio::test]
async fn retry_sweeps_content_written_after_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    // Create a key before erasure so a key row exists for the late insert below.
    get_api_key_for_org_with_session(&server, org.to_string(), &session).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    // Simulate an in-flight stream landing after commit.
    let client = database.pool().get().await.unwrap();
    let r = client.query_one(
        "SELECT w.id, k.id FROM workspaces w JOIN api_keys k ON k.workspace_id = w.id WHERE w.organization_id = $1 LIMIT 1",
        &[&org],
    ).await.unwrap();
    let (ws, key): (uuid::Uuid, uuid::Uuid) = (r.get(0), r.get(1));
    client.execute(
        "INSERT INTO conversations (id, workspace_id, api_key_id, metadata) VALUES ($1, $2, $3, '{}'::jsonb)",
        &[&uuid::Uuid::new_v4(), &ws, &key],
    ).await.unwrap();

    let again = erase(&server, user_id, "x")
        .await
        .json::<serde_json::Value>();
    assert_eq!(again["already_erased"], true);
    assert_eq!(count(&client, "SELECT COUNT(*) FROM conversations WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)", &org).await, 0);
}

#[tokio::test]
async fn retry_sweep_deletes_late_refresh_token() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    // A login that raced the erase leaves a live refresh token behind.
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO refresh_tokens (id, user_id, token_hash, created_at, expires_at, ip_address, user_agent) \
             VALUES ($1, $2, $3, NOW(), NOW() + INTERVAL '1 day', NULL, 'late-login')",
            &[
                &uuid::Uuid::new_v4(),
                &user_id,
                &format!("late-{}", uuid::Uuid::new_v4()),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        1
    );

    let again = erase(&server, user_id, "x")
        .await
        .json::<serde_json::Value>();
    assert_eq!(again["already_erased"], true);
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn erase_scrubs_denormalized_emails_tokens_votes_and_invitations() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_admin_token(&client, user_id, false).await;
    client.execute(
        "INSERT INTO organization_limits_history (organization_id, spend_limit, effective_from, changed_by_user_id, changed_by_user_email, credit_type, source, currency) \
         VALUES ($1, 0, NOW(), $2, $3, 'grant', 'erasure-test', 'USD')",
        &[&org, &user_id, &email],
    ).await.unwrap();
    let outsider = format!("outsider-{}@test.com", uuid::Uuid::new_v4());
    client.execute(
        "INSERT INTO organization_invitations (organization_id, email, role, invited_by_user_id, token, expires_at) \
         VALUES ($1, $2, 'member', $3, $4, NOW() + INTERVAL '7 days')",
        &[&org, &outsider, &user_id, &uuid::Uuid::new_v4().to_string()],
    ).await.unwrap();
    let target: uuid::Uuid = client.query_one(
        "INSERT INTO feature_request_targets (kind, key, title) VALUES ('feature', $1, 'erasure test feature') RETURNING id",
        &[&format!("erase-{}", uuid::Uuid::new_v4())],
    ).await.unwrap().get(0);
    client
        .execute(
            "INSERT INTO feature_request_votes (target_id, user_id) VALUES ($1, $2)",
            &[&target, &user_id],
        )
        .await
        .unwrap();

    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    let tomb = format!("erased+{user_id}@erased.invalid");
    let limits_email: Option<String> = client
        .query_one("SELECT changed_by_user_email FROM organization_limits_history WHERE changed_by_user_id = $1 LIMIT 1", &[&user_id])
        .await.unwrap().get(0);
    assert_eq!(limits_email.as_deref(), Some(tomb.as_str()));
    let token = client.query_one(
        "SELECT name, creation_reason, revocation_reason, user_agent FROM admin_access_token WHERE created_by_user_id = $1", &[&user_id],
    ).await.unwrap();
    assert_eq!(token.get::<_, String>("name"), "erased");
    assert_eq!(
        token
            .get::<_, Option<String>>("revocation_reason")
            .as_deref(),
        Some("erased")
    );
    assert_eq!(token.get::<_, Option<String>>("user_agent"), None);
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_invitations WHERE organization_id = $1",
            &org
        )
        .await,
        0
    );
    // The erased user's vote is removed, but the target is shared content that
    // other users may have voted on, so it is retained.
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM feature_request_votes WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM feature_request_targets WHERE id = $1",
            &target
        )
        .await,
        1
    );
}

#[tokio::test]
async fn erase_disconnects_staking_source_of_erased_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    insert_staking_source(&client, org, user_id, "[]", 1).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    let status: String = client
        .query_one(
            "SELECT status FROM organization_staking_farm_sources WHERE organization_id = $1",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "disconnected");
}

#[tokio::test]
async fn retry_does_not_sweep_retained_team_org() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_key = get_api_key_for_org_with_session(&server, team.id.clone(), &session).await;
    let created = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {team_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(created.status_code(), 201, "{}", created.text());
    transfer_ownership(&server, &team.id, teammate).await;

    assert_eq!(erase(&server, owner, &email).await.status_code(), 200);
    let again = erase(&server, owner, &email).await;
    assert_eq!(again.status_code(), 200, "{}", again.text());
    assert_eq!(again.json::<serde_json::Value>()["already_erased"], true);

    let client = database.pool().get().await.unwrap();
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM conversations WHERE workspace_id IN \
             (SELECT id FROM workspaces WHERE organization_id = $1)",
            &team_uuid
        )
        .await,
        1,
        "the retained org's content must survive the retry sweep"
    );
    assert!(client
        .query_one(
            "SELECT is_active FROM organizations WHERE id = $1",
            &[&team_uuid]
        )
        .await
        .unwrap()
        .get::<_, bool>(0));
}

#[tokio::test]
async fn erasure_log_records_email_digest_and_org_ids() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let (other_session, _other, _) = new_user(&database).await;
    let personal: uuid::Uuid = personal_org_id(&server, &session).await.parse().unwrap();
    let team = create_org_with_session(&server, &other_session).await;
    add_member(&database, &team.id, user_id, "member").await;
    let team: uuid::Uuid = team.id.parse().unwrap();

    let mixed_case = format!("  {}  ", email.to_uppercase());
    assert_eq!(
        erase(&server, user_id, &mixed_case).await.status_code(),
        200
    );

    let client = database.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT email_sha256, erased_organization_ids, retained_organization_ids, \
             erased_organization_count FROM user_erasure_log WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let digest: Vec<u8> = row.get("email_sha256");
    assert_eq!(
        digest,
        services::user_erasure::erased_email_digest(&email.to_uppercase()).to_vec()
    );
    assert_eq!(
        row.get::<_, Vec<uuid::Uuid>>("erased_organization_ids"),
        vec![personal]
    );
    assert_eq!(
        row.get::<_, Vec<uuid::Uuid>>("retained_organization_ids"),
        vec![team]
    );
    assert_eq!(row.get::<_, i32>("erased_organization_count"), 1);

    let leaked: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM user_erasure_log l WHERE l.user_id = $1 AND position($2 in lower(l::text)) > 0",
            &[&user_id, &email.to_lowercase()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(leaked, 0, "the erasure log must not contain the raw email");
}
