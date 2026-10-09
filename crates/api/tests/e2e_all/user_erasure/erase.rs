use super::*;

#[tokio::test]
async fn erase_personal_org_user_removes_content_and_identity_keeps_usage() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let org_id = personal_org_id(&server, &session).await;
    let org: uuid::Uuid = org_id.parse().unwrap();
    let api_key = get_api_key_for_org_with_session(&server, org_id.clone(), &session).await;
    let client = database.pool().get().await.unwrap();

    make_usage(&server, &org_id, &api_key).await;
    let usage_before = wait_for_usage(&client, org).await;
    assert!(usage_before > 0, "fixture must produce a usage row");

    let conv = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conv.status_code(), 201, "{}", conv.text());
    let file = server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", "user_data")
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(b"secret".to_vec())
                        .file_name("notes.txt")
                        .mime_type("text/plain"),
                ),
        )
        .await;
    assert_eq!(file.status_code(), 201, "{}", file.text());

    let response = erase(&server, user_id, &email).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["lifecycle"], "erased");
    assert_eq!(body["already_erased"], false);
    assert_eq!(body["erased_organization_ids"][0], org_id);

    let after = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(after.status_code(), 401, "{}", after.text());

    let in_org = "SELECT COUNT(*) FROM {} WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)";
    for table in ["conversations", "responses", "files"] {
        assert_eq!(
            count(&client, &in_org.replace("{}", table), &org).await,
            0,
            "{table}"
        );
    }
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_usage_log WHERE organization_id = $1",
            &org
        )
        .await,
        usage_before,
        "billing ledger survives erasure"
    );

    let u = client
        .query_one(
            "SELECT email, username, display_name, auth_provider, provider_user_id, is_active FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(
        u.get::<_, String>("email"),
        format!("erased+{user_id}@erased.invalid")
    );
    assert_eq!(u.get::<_, String>("username"), "erased");
    assert_eq!(u.get::<_, Option<String>>("display_name"), None);
    assert_eq!(u.get::<_, String>("auth_provider"), "erased");
    assert_eq!(u.get::<_, String>("provider_user_id"), user_id.to_string());
    assert!(!u.get::<_, bool>("is_active"));

    let o = client
        .query_one(
            "SELECT name, description, settings, is_active FROM organizations WHERE id = $1",
            &[&org],
        )
        .await
        .unwrap();
    assert_eq!(o.get::<_, String>("name"), format!("erased-{org}"));
    assert_eq!(o.get::<_, Option<String>>("description"), None);
    assert!(o.get::<_, Option<serde_json::Value>>("settings").is_none());
    assert!(!o.get::<_, bool>("is_active"));
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1",
            &org
        )
        .await,
        1
    );
    let prefixes: Vec<Option<String>> = client
        .query(
            "SELECT key_prefix FROM api_keys WHERE workspace_id IN (SELECT id FROM workspaces WHERE organization_id = $1)",
            &[&org],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(prefixes.iter().all(|p| p.as_deref() == Some("sk-****")));

    let log = client
        .query_one(
            "SELECT erased_organization_count FROM user_erasure_log WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(log.get::<_, i32>("erased_organization_count"), 1);
}

#[tokio::test]
async fn erased_user_can_sign_up_again_with_same_identity() {
    let (server, database) = setup_test_server_with_database().await;
    let u = uuid::Uuid::new_v4();
    let provider_id = format!("gh-{u}");
    let email = format!("again-{u}@test.com");
    let (_s, first, _) = signup(&database, "github", &provider_id, &email).await;
    assert_eq!(erase(&server, first, &email).await.status_code(), 200);
    let (_s2, second, _) = signup(&database, "github", &provider_id, &email).await;
    assert_ne!(first, second, "re-signup must create a fresh account");
}

#[tokio::test]
async fn erase_rejects_mismatched_email_and_accepts_case_and_whitespace_variants() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let wrong = erase(&server, user_id, "someone-else@test.com").await;
    assert_eq!(wrong.status_code(), 422, "{}", wrong.text());
    assert_eq!(
        wrong.json::<serde_json::Value>()["error"]["type"],
        "confirm_email_mismatch"
    );
    assert_eq!(
        erase(&server, user_id, &format!("  {}  ", email.to_uppercase()))
            .await
            .status_code(),
        200
    );
}

#[tokio::test]
async fn erase_again_reports_already_erased() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let first = erase(&server, user_id, &email)
        .await
        .json::<serde_json::Value>();
    let again = erase(&server, user_id, "anything").await;
    assert_eq!(again.status_code(), 200, "{}", again.text());
    let again = again.json::<serde_json::Value>();
    assert_eq!(again["already_erased"], true);
    assert_eq!(
        again["erased_organization_ids"],
        first["erased_organization_ids"]
    );
}

#[tokio::test]
async fn erase_after_ownership_transfer_keeps_team_org_and_keys() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, owner_email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_key = get_api_key_for_org_with_session(&server, team.id.clone(), &session).await;
    transfer_ownership(&server, &team.id, teammate).await;

    assert_eq!(erase(&server, owner, &owner_email).await.status_code(), 200);

    let client = database.pool().get().await.unwrap();
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1",
            &team_uuid
        )
        .await,
        1
    );
    let ok = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {team_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(
        ok.status_code(),
        201,
        "a key created in a retained org belongs to the org: {}",
        ok.text()
    );
}

#[tokio::test]
async fn erase_renames_retained_org_still_named_after_user_email() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, owner_email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let personal = personal_org_id(&server, &session).await;
    add_member(&database, &personal, teammate, "member").await;
    transfer_ownership(&server, &personal, teammate).await;
    let client = database.pool().get().await.unwrap();
    let org: uuid::Uuid = personal.parse().unwrap();
    let old: String = client
        .query_one("SELECT name FROM organizations WHERE id = $1", &[&org])
        .await
        .unwrap()
        .get(0);
    assert!(old.starts_with(&format!("{}-org-", owner_email.split('@').next().unwrap())));

    assert_eq!(erase(&server, owner, &owner_email).await.status_code(), 200);

    let name: String = client
        .query_one("SELECT name FROM organizations WHERE id = $1", &[&org])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, format!("org-{org}"));
    let desc: Option<String> = client
        .query_one(
            "SELECT description FROM workspaces WHERE organization_id = $1 AND name = 'default'",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        desc.as_deref(),
        Some(format!("Default workspace for org-{org}").as_str())
    );
}

#[tokio::test]
async fn erase_renames_retained_signup_org_after_email_change() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, _signup_email) = new_user(&database).await;
    let (teammate_session, teammate, _) = new_user(&database).await;
    let personal = personal_org_id(&server, &session).await;
    add_member(&database, &personal, teammate, "member").await;
    transfer_ownership(&server, &personal, teammate).await;
    // U also belongs to the teammate's own signup org, which has the same name
    // shape but a default workspace U did not create.
    let teammate_org = personal_org_id(&server, &teammate_session).await;
    add_member(&database, &teammate_org, owner, "member").await;

    let client = database.pool().get().await.unwrap();
    let new_email = format!("changed-{}@test.com", uuid::Uuid::new_v4());
    client
        .execute(
            "UPDATE users SET email = $2 WHERE id = $1",
            &[&owner, &new_email],
        )
        .await
        .unwrap();
    let org: uuid::Uuid = personal.parse().unwrap();
    let other_org: uuid::Uuid = teammate_org.parse().unwrap();
    let other_name_before: String = client
        .query_one(
            "SELECT name FROM organizations WHERE id = $1",
            &[&other_org],
        )
        .await
        .unwrap()
        .get(0);

    assert_eq!(erase(&server, owner, &new_email).await.status_code(), 200);

    let name: String = client
        .query_one("SELECT name FROM organizations WHERE id = $1", &[&org])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, format!("org-{org}"));
    let desc: Option<String> = client
        .query_one(
            "SELECT description FROM workspaces WHERE organization_id = $1 AND name = 'default'",
            &[&org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        desc.as_deref(),
        Some(format!("Default workspace for org-{org}").as_str())
    );

    let other_name_after: String = client
        .query_one(
            "SELECT name FROM organizations WHERE id = $1",
            &[&other_org],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        other_name_after, other_name_before,
        "another user's signup org must not be renamed"
    );
}

#[tokio::test]
async fn erase_leaves_owner_deleted_shared_org_without_blocking() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, owner, email) = new_user(&database).await;
    let (_s2, teammate, _) = new_user(&database).await;
    let team = create_org_with_session(&server, &session).await;
    add_member(&database, &team.id, teammate, "member").await;
    let team_uuid: uuid::Uuid = team.id.parse().unwrap();
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&team_uuid],
        )
        .await
        .unwrap();

    assert_eq!(erase(&server, owner, &email).await.status_code(), 200);
    let left: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&team_uuid, &owner],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);
}

#[tokio::test]
async fn erase_user_with_no_memberships() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "DELETE FROM organization_members WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let r = erase(&server, user_id, &email).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    assert_eq!(
        r.json::<serde_json::Value>()["erased_organization_ids"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let stored: String = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one("SELECT email FROM users WHERE id = $1", &[&user_id])
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, format!("erased+{user_id}@erased.invalid"));
}

#[tokio::test]
async fn near_user_can_log_in_again_after_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let account = format!("erase-{}.testnet", uuid::Uuid::new_v4().simple());
    let email = format!("{account}@near");
    let (_s, first, _) = signup(&database, "near", &account, &email).await;
    assert_eq!(erase(&server, first, &email).await.status_code(), 200);
    let (_s2, second, _) = signup(&database, "near", &account, &email).await;
    assert_ne!(first, second);
}

#[tokio::test]
async fn erase_deactivated_user() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let p = preview(&server, user_id).await;
    assert_eq!(p.status_code(), 200, "{}", p.text());
    let r = erase(&server, user_id, &email).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    let body = r.json::<serde_json::Value>();
    assert_eq!(body["lifecycle"], "erased");
    assert_eq!(body["already_erased"], false);
}
