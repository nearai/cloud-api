use crate::common::*;
use api::models::{AdminAccessTokenPermission, AdminAccessTokenResponse};
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

async fn issue(
    server: &axum_test::TestServer,
    permission: Option<&str>,
) -> AdminAccessTokenResponse {
    let mut body = json!({
        "name": format!("permissions-{}", Uuid::new_v4()),
        "reason": "Admin permission integration test",
        "expires_in_hours": 24
    });
    if let Some(permission) = permission {
        body["permission"] = json!(permission);
    }
    let response = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    response.json()
}

async fn call(
    server: &axum_test::TestServer,
    method: Method,
    path: &str,
    token: &str,
    body: Value,
) -> axum_test::TestResponse {
    server
        .method(method, path)
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

const ENCRYPTION_KEYS: &str = "/v1/admin/database-encryption/keys";
const ENCRYPTION_JOBS: &str = "/v1/admin/database-encryption/jobs";

fn assert_forbidden(response: &axum_test::TestResponse, error_type: &str) {
    assert_eq!(response.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(response.json::<Value>()["error"]["type"], error_type);
}

#[tokio::test]
async fn permissions_persist_in_create_validate_get_and_list() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    for (requested, expected) in [
        (None, AdminAccessTokenPermission::ReadWrite),
        (Some("read_write"), AdminAccessTokenPermission::ReadWrite),
        (Some("read_only"), AdminAccessTokenPermission::ReadOnly),
    ] {
        let created = issue(&server, requested).await;
        assert_eq!(created.permission, expected);
        assert!(created.access_token.starts_with("adm_"));
        let id = Uuid::parse_str(&created.id).unwrap();
        let stored = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(stored.permission, expected.into());
        assert_ne!(stored.token_hash, created.access_token);
        let validated = repo
            .validate(&created.access_token, Some(MOCK_USER_AGENT))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(validated.permission, expected.into());
        assert!(validated.last_used_at.is_some());
        let listed = call(
            &server,
            Method::GET,
            "/v1/admin/access-tokens?limit=1000",
            &get_session_id(),
            json!({}),
        )
        .await;
        assert_eq!(listed.status_code(), StatusCode::OK);
        let listed = listed.json::<api::models::ListAdminAccessTokensResponse>();
        let entry = listed.data.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(entry.permission, expected);
    }

    for invalid in [Value::Null, json!("admin"), json!("READ_ONLY"), json!(7)] {
        let response = call(
            &server,
            Method::POST,
            "/v1/admin/access-tokens",
            &get_session_id(),
            json!({
                "name": "invalid", "reason": "test", "expires_in_hours": 24, "permission": invalid
            }),
        )
        .await;
        assert!(matches!(
            response.status_code(),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ));
    }
}

#[tokio::test]
async fn issuance_gate_does_not_relax_existing_token_permissions() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = false;
    })
    .await;
    let response = call(
        &server,
        Method::POST,
        "/v1/admin/access-tokens",
        &get_session_id(),
        json!({
            "name": "gated", "reason": "test", "expires_in_hours": 24, "permission": "read_only"
        }),
    )
    .await;
    assert_eq!(response.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "read_only_token_issuance_disabled"
    );
    assert_eq!(
        issue(&server, None).await.permission,
        AdminAccessTokenPermission::ReadWrite
    );
    assert_eq!(
        issue(&server, Some("read_write")).await.permission,
        AdminAccessTokenPermission::ReadWrite
    );

    // Model a token issued before the gate was disabled, without enabling it on this server.
    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    let (_, token) = repo
        .create(
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            "existing read-only token".into(),
            "test".into(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some(MOCK_USER_AGENT.into()),
            database::models::AdminAccessTokenPermission::ReadOnly,
        )
        .await
        .unwrap();
    assert_eq!(
        call(
            &server,
            Method::GET,
            "/v1/admin/api-keys",
            &token,
            json!({})
        )
        .await
        .status_code(),
        StatusCode::OK
    );
    assert_forbidden(
        &call(&server, Method::POST, ENCRYPTION_JOBS, &token, json!({})).await,
        "insufficient_permissions",
    );
}

#[tokio::test]
async fn admin_tokens_only_accepts_admin_tokens_and_refuses_sessions() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_tokens_only = true;
    })
    .await;
    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    let (_, token) = repo
        .create(
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            "admin tokens only".into(),
            "test".into(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some(MOCK_USER_AGENT.into()),
            database::models::AdminAccessTokenPermission::ReadWrite,
        )
        .await
        .unwrap();

    let response = call(
        &server,
        Method::GET,
        "/v1/admin/api-keys",
        &token,
        json!({}),
    )
    .await;
    assert_eq!(response.status_code(), StatusCode::OK);

    for (method, path) in [
        (Method::GET, "/v1/admin/api-keys"),
        (Method::POST, "/v1/admin/access-tokens"),
    ] {
        let response = call(
            &server,
            method,
            path,
            &get_session_id(),
            json!({"name": "refused", "reason": "test", "expires_in_hours": 24}),
        )
        .await;
        assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED, "{path}");
        let body = response.json::<Value>();
        assert_eq!(body["error"]["message"], "Admin access token required");
        assert_eq!(body["error"]["type"], "unauthorized");
    }
}

#[tokio::test]
async fn read_only_reads_and_mixed_method_mutations_have_no_side_effects() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let read_only = issue(&server, Some("read_only")).await.access_token;
    let read_write = issue(&server, Some("read_write")).await.access_token;

    let job_count = || async {
        db.pool()
            .get()
            .await
            .unwrap()
            .query_one("SELECT COUNT(*) FROM database_encryption_jobs", &[])
            .await
            .unwrap()
            .get::<_, i64>(0)
    };

    let keys_before = call(&server, Method::GET, ENCRYPTION_KEYS, &read_only, json!({})).await;
    assert_eq!(keys_before.status_code(), StatusCode::OK);
    let keys_before = keys_before.json::<Value>();
    let jobs_before = job_count().await;
    let scope = json!({"fields": [{"table": "files", "column": "filename"}]});
    for (method, route, body) in [
        (
            Method::POST,
            ENCRYPTION_JOBS.to_string(),
            json!({"mode": "execute", "scope": scope, "actions": ["rewrap"]}),
        ),
        (
            Method::POST,
            "/v1/admin/database-encryption/verify".into(),
            json!({"scope": scope, "mode": "verify", "actions": ["verify"], "batch_size": 1}),
        ),
        (
            Method::POST,
            format!(
                "/v1/admin/database-encryption/jobs/{}/cancel",
                Uuid::new_v4()
            ),
            json!({}),
        ),
        (
            Method::PUT,
            format!(
                "/v1/admin/organizations/{}/members/{}",
                org.id, MOCK_USER_ID
            ),
            json!({"role": "member"}),
        ),
        (
            Method::POST,
            format!("/v1/admin/organizations/{}/staking/farm/sync", org.id),
            json!({}),
        ),
        (
            Method::POST,
            "/v1/admin/usage-hourly/recompute".into(),
            json!({"start": "1900-01-01T00:00:00Z", "end": "1900-01-01T01:00:00Z"}),
        ),
    ] {
        assert_forbidden(
            &call(&server, method, &route, &read_only, body).await,
            "insufficient_permissions",
        );
    }
    let after = call(&server, Method::GET, ENCRYPTION_KEYS, &read_only, json!({})).await;
    assert_eq!(after.json::<Value>(), keys_before);
    assert_eq!(job_count().await, jobs_before);

    for route in [
        "/v1/admin/api-keys",
        ENCRYPTION_KEYS,
        "/v1/admin/object-encryption/keys",
        "/v1/admin/invitation-email-deliveries",
    ] {
        assert_eq!(
            call(&server, Method::GET, route, &read_only, json!({}))
                .await
                .status_code(),
            StatusCode::OK,
            "{route}"
        );
    }
    assert_eq!(
        call(
            &server,
            Method::HEAD,
            ENCRYPTION_KEYS,
            &read_only,
            json!({})
        )
        .await
        .status_code(),
        StatusCode::OK
    );

    for token in [&read_write, &get_session_id()] {
        assert_eq!(
            call(
                &server,
                Method::POST,
                "/v1/admin/database-encryption/scan",
                token,
                json!({"scope": scope, "limit": 1})
            )
            .await
            .status_code(),
            StatusCode::OK
        );
    }

    for token in [&read_only, &read_write] {
        for (method, route, body) in [
            (Method::GET, "/v1/admin/access-tokens".into(), json!({})),
            (
                Method::POST,
                "/v1/admin/access-tokens".into(),
                json!({"name": "forbidden", "reason": "test", "expires_in_hours": 1}),
            ),
            (
                Method::DELETE,
                format!("/v1/admin/access-tokens/{}", Uuid::new_v4()),
                json!({"reason": "test"}),
            ),
        ] {
            assert_forbidden(
                &call(&server, method, &route, token, body).await,
                "forbidden",
            );
        }
    }
}

#[tokio::test]
async fn database_encryption_reads_cannot_create_or_cancel_jobs() {
    let user = Uuid::new_v4();
    let creator_email = format!("test-{user}@test.com");
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
        config.auth.admin_emails = vec![creator_email.clone()];
    })
    .await;
    setup_unique_test_session_with_user_id(&db, user).await;

    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    let (_, token) = repo
        .create(
            user,
            "encryption reads".into(),
            "test".into(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some(MOCK_USER_AGENT.into()),
            database::models::AdminAccessTokenPermission::ReadOnly,
        )
        .await
        .unwrap();
    for method in [Method::GET, Method::HEAD] {
        assert_eq!(
            call(
                &server,
                method,
                "/v1/admin/database-encryption/keys",
                &token,
                json!({})
            )
            .await
            .status_code(),
            StatusCode::OK
        );
    }
    for mode in ["execute", "dry_run"] {
        assert_forbidden(
            &call(&server, Method::POST, "/v1/admin/database-encryption/jobs", &token,
                json!({"mode":mode,"scope":{"fields":[{"table":"files","column":"filename"}]},"actions":["rewrap"]})).await,
            "insufficient_permissions",
        );
    }
    let scope = json!({"fields": [{"table": "files", "column": "filename"}]});
    let scan = call(
        &server,
        Method::POST,
        "/v1/admin/database-encryption/scan",
        &token,
        json!({"scope": scope, "limit": 1}),
    )
    .await;
    assert_eq!(scan.status_code(), StatusCode::OK);
    let id = Uuid::new_v4();
    let client = db.pool().get().await.unwrap();
    client.execute("INSERT INTO database_encryption_jobs (id, mode, status, scope, actions, batch_size, admin_actor) VALUES ($1, 'verify', 'completed', $2, '[\"verify\"]', 1, $3)", &[&id, &scope, &user]).await.unwrap();
    let route = format!("/v1/admin/database-encryption/jobs/{id}");
    assert_eq!(
        call(&server, Method::GET, &route, &token, json!({}))
            .await
            .status_code(),
        StatusCode::OK
    );
    let body = json!({"scope": scope, "mode": "verify", "actions": ["verify"], "batch_size": 1});
    for path in [
        "/v1/admin/database-encryption/jobs".to_string(),
        "/v1/admin/database-encryption/verify".to_string(),
        format!("{route}/cancel"),
    ] {
        assert_forbidden(
            &call(&server, Method::POST, &path, &token, body.clone()).await,
            "insufficient_permissions",
        );
    }
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM database_encryption_jobs WHERE admin_actor = $1",
            &[&user],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "scan and denied writes must not create jobs");
    let row = client
        .query_one(
            "SELECT status, cancel_requested_at FROM database_encryption_jobs WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "completed");
    assert!(row
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>(1)
        .is_none());
}

#[tokio::test]
async fn unclassified_routes_deny_reads_and_preserve_authentication_context() {
    use api::middleware::auth::{admin_middleware, AdminAuthContext, AdminUser};
    use axum::{middleware::from_fn_with_state, routing::get, Extension, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let read_only = issue(&server, Some("read_only")).await;
    let read_write = issue(&server, Some("read_write")).await;
    let entered = Arc::new(AtomicUsize::new(0));
    let count = entered.clone();
    let probe = move |Extension(context): Extension<AdminAuthContext>,
                      Extension(_): Extension<AdminUser>| {
        count.fetch_add(1, Ordering::SeqCst);
        async move {
            axum::Json(match context {
                AdminAuthContext::Session => json!({"source": "session"}),
                AdminAuthContext::AccessToken {
                    token_id,
                    permission,
                } => json!({"source": "token", "id": token_id, "permission": permission}),
            })
        }
    };
    let auth = api::init_auth_services(db, &test_config()).auth_state_middleware;
    let routes = Router::new()
        .route("/admin/unreviewed", get(probe.clone()).post(probe.clone()))
        // The same URL as an approved POST must not accidentally approve a new GET.
        .route("/admin/database-encryption/scan", get(probe.clone()))
        .route("/admin/database-encryption/jobs/{id}", get(probe))
        .layer(from_fn_with_state(auth, admin_middleware));
    let server = axum_test::TestServer::new(Router::new().nest("/v1", routes));
    for (method, path) in [
        (Method::GET, "/v1/admin/unreviewed"),
        (Method::POST, "/v1/admin/unreviewed"),
        (Method::GET, "/v1/admin/database-encryption/scan"),
    ] {
        assert_forbidden(
            &call(&server, method, path, &read_only.access_token, json!({})).await,
            "insufficient_permissions",
        );
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        0,
        "denied requests must not enter handlers"
    );
    let context = call(
        &server,
        Method::GET,
        "/v1/admin/database-encryption/jobs/some-job",
        &read_only.access_token,
        json!({}),
    )
    .await;
    assert_eq!(context.status_code(), StatusCode::OK);
    assert_eq!(
        context.json::<Value>(),
        json!({"source": "token", "id": read_only.id, "permission": "read_only"})
    );
    let write = call(
        &server,
        Method::POST,
        "/v1/admin/unreviewed",
        &read_write.access_token,
        json!({}),
    )
    .await;
    assert_eq!(write.status_code(), StatusCode::OK);
    assert_eq!(write.json::<Value>()["permission"], "read_write");
    let session = call(
        &server,
        Method::GET,
        "/v1/admin/unreviewed",
        &get_session_id(),
        json!({}),
    )
    .await;
    assert_eq!(session.status_code(), StatusCode::OK);
    assert_eq!(session.json::<Value>()["source"], "session");
}

#[tokio::test]
async fn permissions_preserve_expiration_revocation_user_agent_and_admin_eligibility() {
    // Use the real auth service so creator eligibility is loaded from the DB,
    // rather than the mock auth service's fixed admin user.
    let user = Uuid::new_v4();
    let creator_email = format!("test-{user}@test.com");
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
        config.auth.admin_emails = vec![creator_email.clone()];
    })
    .await;
    setup_unique_test_session_with_user_id(&db, user).await;

    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    for permission in [
        database::models::AdminAccessTokenPermission::ReadOnly,
        database::models::AdminAccessTokenPermission::ReadWrite,
    ] {
        let (created, token) = repo
            .create(
                user,
                "lifecycle".into(),
                "test".into(),
                chrono::Utc::now() + chrono::Duration::hours(1),
                Some(MOCK_USER_AGENT.into()),
                permission,
            )
            .await
            .unwrap();
        assert_eq!(
            call(
                &server,
                Method::GET,
                "/v1/admin/api-keys",
                &token,
                json!({})
            )
            .await
            .status_code(),
            StatusCode::OK
        );
        for user_agent in [None, Some("different client")] {
            let mut request = server
                .get("/v1/admin/api-keys")
                .add_header("Authorization", format!("Bearer {token}"));
            if let Some(ua) = user_agent {
                request = request.add_header("User-Agent", ua);
            }
            assert_eq!(request.await.status_code(), StatusCode::UNAUTHORIZED);
        }
        let client = db.pool().get().await.unwrap();
        client
            .execute(
                "UPDATE users SET email = $1 WHERE id = $2",
                &[&format!("{user}@not-admin.example"), &user],
            )
            .await
            .unwrap();
        assert_forbidden(
            &call(
                &server,
                Method::GET,
                "/v1/admin/api-keys",
                &token,
                json!({}),
            )
            .await,
            "forbidden",
        );
        client
            .execute(
                "UPDATE users SET email = $1 WHERE id = $2",
                &[&creator_email, &user],
            )
            .await
            .unwrap();
        drop(client);
        assert!(repo
            .revoke(created.id, user, "test revocation".into())
            .await
            .unwrap());
        assert_eq!(
            call(
                &server,
                Method::GET,
                "/v1/admin/api-keys",
                &token,
                json!({})
            )
            .await
            .status_code(),
            StatusCode::UNAUTHORIZED
        );
        let (_, expired) = repo
            .create(
                user,
                "expired".into(),
                "test".into(),
                chrono::Utc::now() - chrono::Duration::hours(1),
                Some(MOCK_USER_AGENT.into()),
                permission,
            )
            .await
            .unwrap();
        assert_eq!(
            call(
                &server,
                Method::GET,
                "/v1/admin/api-keys",
                &expired,
                json!({})
            )
            .await
            .status_code(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            &server,
            Method::GET,
            "/v1/admin/api-keys",
            "adm_invalid",
            json!({})
        )
        .await
        .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn existing_service_tokens_follow_the_creator_email_allowlist() {
    let (allowed_server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_emails = vec![ADMIN_ALLOWLIST_EMAIL.to_ascii_uppercase()];
    })
    .await;
    let repo = database::repositories::AdminAccessTokenRepository::new(db.pool().clone());
    let (_, token) = repo
        .create(
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            format!("allowlist-{}", Uuid::new_v4()),
            "Existing service credential".to_string(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some(MOCK_USER_AGENT.to_string()),
            database::models::AdminAccessTokenPermission::ReadWrite,
        )
        .await
        .unwrap();

    let response = call(
        &allowed_server,
        Method::GET,
        "/v1/admin/api-keys",
        &token,
        json!({}),
    )
    .await;

    assert_eq!(response.status_code(), StatusCode::OK);

    // The shared database retains the same token; only the server allowlist changes.
    for emails in [vec![NON_ADMIN_ALLOWLIST_EMAIL.to_string()], Vec::new()] {
        let denied_server = setup_test_server_with_config(|config| {
            config.auth.admin_emails = emails;
        })
        .await;

        for credential in [&token, &get_session_id()] {
            let response = call(
                &denied_server,
                Method::GET,
                "/v1/admin/api-keys",
                credential,
                json!({}),
            )
            .await;

            assert_forbidden(&response, "forbidden");
        }

        // Ordinary authenticated user endpoints remain available.
        let response = call(
            &denied_server,
            Method::GET,
            "/v1/users/me",
            &get_session_id(),
            json!({}),
        )
        .await;

        assert_eq!(response.status_code(), StatusCode::OK);
    }

    let response = call(
        &allowed_server,
        Method::GET,
        "/v1/admin/api-keys",
        &token,
        json!({}),
    )
    .await;

    assert_eq!(response.status_code(), StatusCode::OK);
}
