//! Deleting an organization must not leave its workspaces and API keys behind.
//!
//! Regression: org deletion only flipped `organizations.is_active`. The org
//! disappeared from `/v1/users/me`, but its workspaces kept showing up there —
//! so a client that derives the selected org from the workspace list pinned
//! itself to the deleted org and every follow-up call 404'd. The org's API key
//! rows also stayed active, including on routes that authenticated the key
//! without resolving its workspace and organization.

use crate::common::*;

/// After deleting an organization, `/v1/users/me` must stop listing both the
/// organization and the workspaces underneath it.
#[tokio::test]
async fn test_delete_organization_removes_its_workspaces_from_users_me() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;

    let deleted_org = create_org_with_session(&server, &session_id).await;
    let deleted_workspace_id =
        list_workspaces_with_session(&server, deleted_org.id.clone(), &session_id)
            .await
            .first()
            .expect("new org should have a default workspace")
            .id
            .clone();

    // Sanity check: before deletion the workspace is visible.
    let me = get_me(&server, &session_id).await;
    assert!(
        workspace_ids(&me).contains(&deleted_workspace_id),
        "workspace should be listed while its org is alive"
    );

    let response = server
        .delete(format!("/v1/organizations/{}", deleted_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    // The replacement org the user creates after the deletion.
    let new_org = create_org_with_session(&server, &session_id).await;

    let me = get_me(&server, &session_id).await;
    assert!(
        !organization_ids(&me).contains(&deleted_org.id),
        "deleted org must not be listed"
    );
    assert!(
        !workspace_ids(&me).contains(&deleted_workspace_id),
        "workspace of a deleted org must not be listed — clients pin the selected org to it"
    );
    assert!(
        me["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .all(|w| w["organization_id"].as_str() == Some(new_org.id.as_str())),
        "every listed workspace must belong to a live org the user is still a member of"
    );
}

/// Every credential under a deleted organization must stop working. The cascade
/// marks persisted credentials revoked, while the API-key and reporting-token
/// validation paths also require an active parent as defense in depth.
#[tokio::test]
async fn test_delete_organization_revokes_its_credentials() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;

    let org = create_org_with_session(&server, &session_id).await;
    let workspace_id = list_workspaces_with_session(&server, org.id.clone(), &session_id)
        .await
        .first()
        .expect("new org should have a default workspace")
        .id
        .clone();
    let api_key = get_api_key_for_org_with_session(&server, org.id.clone(), &session_id).await;
    let reporting_token = create_reporting_token(&server, &org.id, &session_id).await;

    // Exercise a route that accepts only an API key before deleting the org.
    // The same key-only path must fail once the parent is gone.
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .delete(format!("/v1/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "a cached API key of a deleted org must not authenticate on a key-only route: {}",
        response.text()
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let active_keys: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM api_keys WHERE workspace_id = $1 AND is_active = true",
            &[&uuid::Uuid::parse_str(&workspace_id).unwrap()],
        )
        .await
        .expect("failed to count api keys")
        .get(0);
    assert_eq!(
        active_keys, 0,
        "API keys under a deleted org must be deactivated, not left marked active"
    );

    // A normal revoke stamps `deleted_at`, and the listing queries key off it —
    // the cascade has to match, not just flip `is_active`.
    let undeleted_keys: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM api_keys WHERE workspace_id = $1 AND deleted_at IS NULL",
            &[&uuid::Uuid::parse_str(&workspace_id).unwrap()],
        )
        .await
        .expect("failed to count api keys")
        .get(0);
    assert_eq!(
        undeleted_keys, 0,
        "API keys under a deleted org must be stamped deleted_at, like a normal revoke"
    );

    let active_workspaces: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM workspaces WHERE id = $1 AND is_active = true",
            &[&uuid::Uuid::parse_str(&workspace_id).unwrap()],
        )
        .await
        .expect("failed to count workspaces")
        .get(0);
    assert_eq!(
        active_workspaces, 0,
        "workspaces under a deleted org must be deactivated"
    );

    // The delete cascade revokes the persisted token, while validation also
    // requires its organization to remain active as defense in depth.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/reporting-token-auth-probe",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {reporting_token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "reporting token of a deleted org must not authenticate: {}",
        response.text()
    );

    let unrevoked_tokens: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_reporting_tokens \
             WHERE organization_id = $1 AND revoked_at IS NULL",
            &[&uuid::Uuid::parse_str(&org.id).unwrap()],
        )
        .await
        .expect("failed to count reporting tokens")
        .get(0);
    assert_eq!(
        unrevoked_tokens, 0,
        "reporting tokens under a deleted org must be revoked"
    );
}

/// Production AuthService caches a successful API-key validation for 30
/// seconds. A cache hit must still be rejected on a key-only route after its
/// organization is deleted.
#[tokio::test]
async fn test_cached_api_key_is_rejected_after_organization_deletion() {
    // Use the normal mock-backed server only to create fixture rows. The
    // separate middleware below uses a real AuthService, constructed after the
    // key exists so its initial bloom-filter load includes the key.
    let (fixture_server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;
    let org = create_org_with_session(&fixture_server, &session_id).await;
    let api_key =
        get_api_key_for_org_with_session(&fixture_server, org.id.clone(), &session_id).await;

    let mut config = test_config();
    config.auth.mock = false;
    let auth_components = api::init_auth_services(database.clone(), &config);
    let auth_server = axum_test::TestServer::new(
        axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .layer(axum::middleware::from_fn_with_state(
                auth_components.auth_state_middleware,
                api::middleware::auth::auth_middleware_with_api_key,
            )),
    );

    // This populates the real AuthService cache.
    let response = auth_server
        .get("/")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());

    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id");
    let organization_repository =
        database::repositories::PgOrganizationRepository::new(database.pool().clone());
    let deleted = services::organization::OrganizationRepository::delete_if_no_staking_farm_source(
        &organization_repository,
        organization_id,
    )
    .await
    .expect("organization deletion should succeed");
    assert_eq!(
        deleted,
        services::organization::DeleteOrganizationResult::Deleted
    );

    // Without the parent lookup in auth_middleware_with_api_key this would use
    // the still-cached key and return 204 until the cache entry expired.
    let response = auth_server
        .get("/")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "a cached key must not authenticate after parent deletion: {}",
        response.text()
    );
}

/// A workspace must not be creatable under a deleted organization. The
/// membership check that guards workspace creation reads
/// `organization_members`, which the deletion does not touch, so it still
/// passes — only the insert's own guard stops the orphan being written. That
/// guard is what closes the race between the check and the insert.
#[tokio::test]
async fn test_workspace_cannot_be_created_under_deleted_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;

    let org = create_org_with_session(&server, &session_id).await;

    let response = server
        .delete(format!("/v1/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "name": "orphan" }))
        .await;
    assert_eq!(
        response.status_code(),
        404,
        "creating a workspace under a deleted org must fail: {}",
        response.text()
    );

    // Straight at the repository, bypassing the service-layer membership check —
    // this is the state the race leaves behind, and only the insert's own guard
    // can reject it.
    let repository = database::repositories::WorkspaceRepository::new(database.pool().clone());
    let result = repository
        .create(
            database::models::CreateWorkspaceRequest {
                name: "raced-orphan".to_string(),
                description: None,
            },
            uuid::Uuid::parse_str(&org.id).unwrap(),
            uuid::Uuid::parse_str(session_id.strip_prefix("rt_").unwrap()).unwrap(),
        )
        .await;
    assert!(
        result.is_err(),
        "the insert itself must refuse a deleted parent org"
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let workspaces: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM workspaces WHERE organization_id = $1 AND is_active = true",
            &[&uuid::Uuid::parse_str(&org.id).unwrap()],
        )
        .await
        .expect("failed to count workspaces")
        .get(0);
    assert_eq!(
        workspaces, 0,
        "no active workspace may exist under a deleted org"
    );
}

/// All child inserts must serialize with the organization row locked by
/// deletion. A plain parent EXISTS can observe the pre-delete committed row;
/// the shared lock makes each insert wait and then recheck `is_active` after
/// the deletion commits.
#[tokio::test]
async fn test_child_creation_serializes_with_organization_deletion() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;
    let org = create_org_with_session(&server, &session_id).await;
    let workspace_id = list_workspaces_with_session(&server, org.id.clone(), &session_id)
        .await
        .first()
        .expect("new org should have a default workspace")
        .id
        .clone();
    let existing_reporting_token = create_reporting_token(&server, &org.id, &session_id).await;

    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id");
    let workspace_id = uuid::Uuid::parse_str(&workspace_id).expect("workspace id");
    let user_id = uuid::Uuid::parse_str(
        session_id
            .strip_prefix("rt_")
            .expect("mock session should contain a user id"),
    )
    .expect("user id");

    let mut deletion_client = database
        .pool()
        .get()
        .await
        .expect("failed to get deletion connection");
    let transaction = deletion_client
        .transaction()
        .await
        .expect("failed to begin deletion transaction");
    transaction
        .query_one(
            "SELECT id FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("failed to lock active organization");

    let start = std::sync::Arc::new(tokio::sync::Barrier::new(4));

    let workspace_task = {
        let pool = database.pool().clone();
        let start = start.clone();
        tokio::spawn(async move {
            start.wait().await;
            database::repositories::WorkspaceRepository::new(pool)
                .create(
                    database::models::CreateWorkspaceRequest {
                        name: "raced-workspace".to_string(),
                        description: None,
                    },
                    organization_id,
                    user_id,
                )
                .await
        })
    };

    let api_key_task = {
        let pool = database.pool().clone();
        let start = start.clone();
        tokio::spawn(async move {
            start.wait().await;
            database::repositories::ApiKeyRepository::new(pool)
                .create(services::workspace::CreateApiKeyRequest {
                    name: "raced-api-key".to_string(),
                    workspace_id: services::workspace::WorkspaceId(workspace_id),
                    created_by_user_id: services::auth::UserId(user_id),
                    expires_at: None,
                    spend_limit: None,
                })
                .await
        })
    };

    let reporting_token_task = {
        let pool = database.pool().clone();
        let start = start.clone();
        tokio::spawn(async move {
            start.wait().await;
            let repository =
                database::repositories::OrganizationReportingTokenRepository::new(pool);
            services::reporting_tokens::OrganizationReportingTokenRepository::create(
                &repository,
                services::reporting_tokens::CreateOrganizationReportingTokenRequest {
                    organization_id,
                    name: "raced-reporting-token".to_string(),
                    created_by_user_id: user_id,
                    expires_at: None,
                },
            )
            .await
        })
    };

    // Release all creates while the deletion transaction holds FOR UPDATE.
    start.wait().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert!(
        !workspace_task.is_finished(),
        "workspace insert must wait on the organization deletion lock"
    );
    assert!(
        !api_key_task.is_finished(),
        "API-key insert must wait on the organization deletion lock"
    );
    assert!(
        !reporting_token_task.is_finished(),
        "reporting-token insert must wait on the organization deletion lock"
    );

    transaction
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&organization_id],
        )
        .await
        .expect("failed to deactivate locked organization");
    transaction
        .commit()
        .await
        .expect("failed to commit deletion transaction");

    assert!(
        workspace_task
            .await
            .expect("workspace task should not panic")
            .is_err(),
        "workspace insert must reject an organization deleted while it waited"
    );
    assert!(
        api_key_task
            .await
            .expect("API-key task should not panic")
            .is_err(),
        "API-key insert must reject an organization deleted while it waited"
    );
    assert!(
        reporting_token_task
            .await
            .expect("reporting-token task should not panic")
            .is_err(),
        "reporting-token insert must reject an organization deleted while it waited"
    );

    // This test deactivated the organization directly to isolate lock behavior,
    // so the pre-existing token remains unrevoked. Validation must still fail
    // closed on the inactive parent.
    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get validation connection");
    let unrevoked_tokens: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_reporting_tokens \
             WHERE organization_id = $1 AND revoked_at IS NULL",
            &[&organization_id],
        )
        .await
        .expect("failed to count unrevoked reporting tokens")
        .get(0);
    assert_eq!(
        unrevoked_tokens, 1,
        "the direct state change intentionally bypasses the delete cascade"
    );

    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/reporting-token-auth-probe",
                org.id
            )
            .as_str(),
        )
        .add_header(
            "Authorization",
            format!("Bearer {existing_reporting_token}"),
        )
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "an unrevoked token of an inactive org must not authenticate: {}",
        response.text()
    );
}

async fn get_me(server: &axum_test::TestServer, session_id: &str) -> serde_json::Value {
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<serde_json::Value>()
}

fn organization_ids(me: &serde_json::Value) -> Vec<String> {
    ids(me, "organizations")
}

fn workspace_ids(me: &serde_json::Value) -> Vec<String> {
    ids(me, "workspaces")
}

fn ids(me: &serde_json::Value, field: &str) -> Vec<String> {
    me[field]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The reported journey, end to end: a brand-new user signs up (which creates
/// their default organization and workspace), deletes that default org, creates
/// a replacement, and then has to be able to actually use it — API keys,
/// members and settings all working, with nothing still pointing at the org
/// they deleted.
#[tokio::test]
async fn test_new_user_can_work_after_replacing_default_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _user_id) = signup_new_user(&database).await;

    // A fresh signup starts with exactly one org and its "default" workspace.
    let me = get_me(&server, &session_id).await;
    let default_org_id = organization_ids(&me)
        .first()
        .expect("signup should create a default organization")
        .clone();
    assert_eq!(
        workspace_ids(&me).len(),
        1,
        "signup should create exactly one workspace"
    );

    // The user deletes the default org...
    let response = server
        .delete(format!("/v1/organizations/{default_org_id}").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    // ...and creates a replacement.
    let new_org = create_org_with_session(&server, &session_id).await;

    // Nothing may still point at the deleted org: this is what pinned clients
    // to a dead organization and made every follow-up call fail.
    let me = get_me(&server, &session_id).await;
    assert_eq!(
        organization_ids(&me),
        vec![new_org.id.clone()],
        "only the replacement org may be listed"
    );
    let workspaces = me["workspaces"].as_array().expect("workspaces array");
    assert_eq!(
        workspaces.len(),
        1,
        "only the replacement org's workspace may be listed"
    );
    assert_eq!(
        workspaces[0]["organization_id"].as_str(),
        Some(new_org.id.as_str()),
        "the listed workspace must belong to the replacement org"
    );

    // The deleted org is gone for good — these are the exact calls that failed.
    for path in [
        format!("/v1/organizations/{default_org_id}/members"),
        format!("/v1/organizations/{default_org_id}/settings"),
    ] {
        let response = server
            .get(path.as_str())
            .add_header("Authorization", format!("Bearer {session_id}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(
            response.status_code(),
            404,
            "deleted org must 404 on {path}: {}",
            response.text()
        );
    }

    // --- API keys work in the replacement org ---
    let workspace_id = list_workspaces_with_session(&server, new_org.id.clone(), &session_id)
        .await
        .first()
        .expect("replacement org should have a default workspace")
        .id
        .clone();

    let api_key = create_api_key_in_workspace_with_session(
        &server,
        workspace_id.clone(),
        "Journey key".to_string(),
        &session_id,
    )
    .await;

    let keys = list_api_keys(&server, &workspace_id, &session_id).await;
    assert_eq!(keys.len(), 1, "created key should be listed");
    assert_eq!(keys[0].id, api_key.id);

    // The key actually authenticates against the data plane. Credits first —
    // otherwise the request is rejected for having no spend limit, which would
    // mask whether authentication itself worked.
    add_credits_with_type(
        &server,
        &new_org.id,
        "payment",
        None,
        10_000_000_000i64,
        "USD",
        &session_id,
    )
    .await;

    let response = server
        .post("/v1/check_api_key")
        .add_header(
            "Authorization",
            format!("Bearer {}", api_key.key.clone().expect("key material")),
        )
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .delete(format!("/v1/workspaces/{workspace_id}/api-keys/{}", api_key.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    assert!(
        list_api_keys(&server, &workspace_id, &session_id)
            .await
            .is_empty(),
        "revoked key should no longer be listed"
    );

    // --- Members work in the replacement org ---
    let members = list_members(&server, &new_org.id, &session_id).await;
    assert_eq!(members.len(), 1, "owner should be the only member");

    let (_other_session, _other_email) = setup_unique_test_session(&database).await;
    let other_user_id = _other_session
        .strip_prefix("rt_")
        .expect("mock session ids are rt_{uuid}")
        .to_string();

    let response = server
        .post(format!("/v1/organizations/{}/members", new_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "user_id": other_user_id, "role": "member" }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        list_members(&server, &new_org.id, &session_id).await.len(),
        2,
        "added member should be listed"
    );

    let response = server
        .delete(format!("/v1/organizations/{}/members/{other_user_id}", new_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    assert_eq!(
        list_members(&server, &new_org.id, &session_id).await.len(),
        1,
        "removed member should be gone"
    );

    // --- Settings work in the replacement org ---
    let response = server
        .get(format!("/v1/organizations/{}/settings", new_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .patch(format!("/v1/organizations/{}/settings", new_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "system_prompt": "Journey prompt" }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response = server
        .get(format!("/v1/organizations/{}/settings", new_org.id).as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let settings = response.json::<serde_json::Value>();
    assert_eq!(
        settings["settings"]["system_prompt"].as_str(),
        Some("Journey prompt"),
        "settings must persist on the replacement org"
    );
}

/// Run the real signup path (the one OAuth and NEAR logins both go through), so
/// the user starts with the default organization and workspace production gives
/// them. The e2e server itself runs with the mock auth service, which maps an
/// `rt_{user_id}` bearer token onto that user.
async fn signup_new_user(database: &std::sync::Arc<database::Database>) -> (String, uuid::Uuid) {
    use services::auth::ports::OAuthUserInfo;

    let mut config = test_config();
    config.auth.mock = false;
    let auth_components = api::init_auth_services(database.clone(), &config);

    let unique = uuid::Uuid::new_v4();
    let user = auth_components
        .auth_service
        .get_or_create_oauth_user(OAuthUserInfo {
            provider: "github".to_string(),
            provider_user_id: format!("gh-{unique}"),
            email: format!("newuser-{unique}@test.com"),
            username: format!("newuser-{unique}"),
            display_name: Some("New User".to_string()),
            avatar_url: None,
        })
        .await
        .expect("signup should create the user");

    (format!("rt_{}", user.id.0), user.id.0)
}

async fn list_api_keys(
    server: &axum_test::TestServer,
    workspace_id: &str,
    session_id: &str,
) -> Vec<api::models::ApiKeyResponse> {
    let response = server
        .get(format!("/v1/workspaces/{workspace_id}/api-keys").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<api::models::ListApiKeysResponse>().api_keys
}

async fn list_members(
    server: &axum_test::TestServer,
    org_id: &str,
    session_id: &str,
) -> Vec<serde_json::Value> {
    let response = server
        .get(format!("/v1/organizations/{org_id}/members").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<serde_json::Value>()["members"]
        .as_array()
        .expect("members array")
        .clone()
}

async fn create_reporting_token(
    server: &axum_test::TestServer,
    org_id: &str,
    session_id: &str,
) -> String {
    let response = server
        .post(format!("/v1/organizations/{org_id}/reporting-tokens").as_str())
        .add_header("Authorization", format!("Bearer {session_id}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": "org deletion cascade",
            "expires_at": (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339(),
        }))
        .await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
    response.json::<serde_json::Value>()["token"]
        .as_str()
        .expect("create response should include the raw token once")
        .to_string()
}
