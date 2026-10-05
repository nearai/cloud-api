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
        call(&server, Method::GET, "/v1/admin/users", &token, json!({}))
            .await
            .status_code(),
        StatusCode::OK
    );
    assert_forbidden(
        &call(
            &server,
            Method::PATCH,
            "/v1/admin/models",
            &token,
            json!({}),
        )
        .await,
        "insufficient_permissions",
    );
}

#[tokio::test]
async fn read_only_reads_and_mixed_method_mutations_have_no_side_effects() {
    let server = setup_test_server_with_config(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let path = format!("/v1/admin/organizations/{}/concurrent-limit", org.id);
    let read_only = issue(&server, Some("read_only")).await.access_token;
    let read_write = issue(&server, Some("read_write")).await.access_token;

    let fallback_path = format!("/v1/admin/organizations/{}/fallback", org.id);
    let fallback_before = call(&server, Method::GET, &fallback_path, &read_only, json!({})).await;
    assert_eq!(fallback_before.status_code(), StatusCode::OK);
    let fallback_before = fallback_before.json::<Value>();
    let before = call(&server, Method::GET, &path, &read_only, json!({})).await;
    assert_eq!(before.status_code(), StatusCode::OK);
    for (method, route, body) in [
        (Method::PATCH, path.clone(), json!({"concurrentLimit": 128})),
        (
            Method::PATCH,
            format!("/v1/admin/organizations/{}/fallback", org.id),
            json!({"enabled": !fallback_before["enabled"].as_bool().unwrap()}),
        ),
        (Method::PATCH, "/v1/admin/models".into(), json!({})),
        (
            Method::POST,
            "/v1/admin/aml/allowlist".into(),
            json!({"accountId": "read-only-test.near", "reason": "test"}),
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
            Method::DELETE,
            "/v1/admin/models/missing-model".into(),
            json!({"reason": "test"}),
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
    let after = call(&server, Method::GET, &path, &read_only, json!({})).await;
    assert_eq!(after.json::<Value>(), before.json::<Value>());
    let fallback = call(
        &server,
        Method::GET,
        &format!("/v1/admin/organizations/{}/fallback", org.id),
        &read_only,
        json!({}),
    )
    .await;
    assert_eq!(fallback.status_code(), StatusCode::OK);
    assert_eq!(fallback.json::<Value>(), fallback_before);

    for route in [
        "/v1/admin/users",
        "/v1/admin/organizations",
        "/v1/admin/models",
        "/v1/admin/feature-requests",
        "/v1/admin/aml/allowlist",
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
        call(&server, Method::HEAD, &path, &read_only, json!({}))
            .await
            .status_code(),
        StatusCode::OK
    );

    for (token, limit) in [(&read_write, 128), (&get_session_id(), 256)] {
        assert_eq!(
            call(
                &server,
                Method::PATCH,
                &path,
                token,
                json!({"concurrentLimit": limit})
            )
            .await
            .status_code(),
            StatusCode::OK
        );
        let updated = call(&server, Method::GET, &path, &read_only, json!({})).await;
        assert_eq!(updated.json::<Value>()["concurrentLimit"], limit);
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
async fn post_previews_are_reads_but_confirmation_is_forbidden() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let token = issue(&server, Some("read_only")).await.access_token;
    let model = format!("permissions/model-{}", Uuid::new_v4());
    let successor = format!("permissions/successor-{}", Uuid::new_v4());
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    for name in [&model, &successor] {
        batch.insert(
            name.clone(),
            serde_json::from_value(json!({
                "inputCostPerToken": {"amount": 1000, "currency": "USD"},
                "outputCostPerToken": {"amount": 2000, "currency": "USD"},
                "modelDisplayName": "Permission test model", "modelDescription": "Test",
                "contextLength": 4096, "maxOutputLength": 1024, "isActive": true
            }))
            .unwrap(),
        );
    }
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let effective = (chrono::Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%dT13:00:00Z")
        .to_string();
    let pricing = json!({"changes": [{"modelId": model, "effectiveAt": effective, "inputCostPerToken": {"amount": 3000, "currency": "USD"}}]});
    let deprecation = json!({"successorModelId": successor, "deprecationDate": effective});
    let dep_path = format!(
        "/v1/admin/models/{}/deprecation",
        urlencoding::encode(&model)
    );
    for (path, body) in [
        ("/v1/admin/models/pricing-changes".to_string(), pricing),
        (dep_path, deprecation),
    ] {
        let preview = call(
            &server,
            Method::POST,
            &format!("{path}/preview"),
            &token,
            body.clone(),
        )
        .await;
        assert_eq!(preview.status_code(), StatusCode::OK, "{}", preview.text());
        assert_forbidden(
            &call(
                &server,
                Method::POST,
                &format!("{path}/confirm"),
                &token,
                body,
            )
            .await,
            "insufficient_permissions",
        );
    }
    let client = db.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT deprecation_date, input_cost_per_token FROM models WHERE model_name = $1",
            &[&model],
        )
        .await
        .unwrap();
    assert!(row
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>(0)
        .is_none());
    assert_eq!(row.get::<_, i64>(1), 1000);
    let scheduled: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM scheduled_model_pricing_changes WHERE model_name = $1",
            &[&model],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(scheduled, 0);
}

#[tokio::test]
async fn database_encryption_reads_cannot_create_or_cancel_jobs() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
    })
    .await;
    let (session, _) = setup_unique_test_session(&db).await;
    let user = Uuid::parse_str(session.strip_prefix("rt_").unwrap()).unwrap();
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
        .route("/admin/models/pricing-changes/preview", get(probe.clone()))
        .route("/admin/models/{model_name}/history", get(probe))
        .layer(from_fn_with_state(auth, admin_middleware));
    let server = axum_test::TestServer::new(Router::new().nest("/v1", routes));
    for (method, path) in [
        (Method::GET, "/v1/admin/unreviewed"),
        (Method::POST, "/v1/admin/unreviewed"),
        (Method::GET, "/v1/admin/models/pricing-changes/preview"),
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
        "/v1/admin/models/vendor%2Fmodel/history",
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
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
    })
    .await;
    let (session, _) = setup_unique_test_session(&db).await;
    let user = Uuid::parse_str(session.strip_prefix("rt_").unwrap()).unwrap();
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
            call(&server, Method::GET, "/v1/admin/users", &token, json!({}))
                .await
                .status_code(),
            StatusCode::OK
        );
        for user_agent in [None, Some("different client")] {
            let mut request = server
                .get("/v1/admin/users")
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
            &call(&server, Method::GET, "/v1/admin/users", &token, json!({})).await,
            "forbidden",
        );
        client
            .execute(
                "UPDATE users SET email = $1 WHERE id = $2",
                &[&format!("{user}@test.com"), &user],
            )
            .await
            .unwrap();
        drop(client);
        assert!(repo
            .revoke(created.id, user, "test revocation".into())
            .await
            .unwrap());
        assert_eq!(
            call(&server, Method::GET, "/v1/admin/users", &token, json!({}))
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
            call(&server, Method::GET, "/v1/admin/users", &expired, json!({}))
                .await
                .status_code(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            &server,
            Method::GET,
            "/v1/admin/users",
            "adm_invalid",
            json!({})
        )
        .await
        .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn migration_backfills_legacy_tokens_and_rejects_invalid_permissions() {
    let (_, db) = setup_test_server_with_database().await;
    let mut client = db.pool().get().await.unwrap();
    let transaction = client.transaction().await.unwrap();
    // Shadow only this connection's table, leaving shared fixtures untouched.
    transaction.batch_execute("CREATE TEMP TABLE admin_access_token (id UUID PRIMARY KEY, token_hash TEXT NOT NULL) ON COMMIT DROP").await.unwrap();
    let id = Uuid::new_v4();
    transaction
        .execute(
            "INSERT INTO admin_access_token (id, token_hash) VALUES ($1, 'unchanged-test-hash')",
            &[&id],
        )
        .await
        .unwrap();
    transaction
        .batch_execute(include_str!(
            "../../../database/src/migrations/sql/V0079__add_admin_access_token_permission.sql"
        ))
        .await
        .unwrap();
    let row = transaction
        .query_one(
            "SELECT token_hash, permission FROM admin_access_token WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "unchanged-test-hash");
    assert_eq!(row.get::<_, String>(1), "read_write");
    for (value, state) in [
        (None, tokio_postgres::error::SqlState::NOT_NULL_VIOLATION),
        (
            Some("unknown"),
            tokio_postgres::error::SqlState::CHECK_VIOLATION,
        ),
    ] {
        transaction
            .batch_execute("SAVEPOINT invalid_permission")
            .await
            .unwrap();
        let err = transaction
            .execute(
                "UPDATE admin_access_token SET permission = $1 WHERE id = $2",
                &[&value, &id],
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(&state));
        transaction
            .batch_execute("ROLLBACK TO SAVEPOINT invalid_permission")
            .await
            .unwrap();
    }
    transaction
        .execute(
            "UPDATE admin_access_token SET permission = 'read_only' WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    transaction.rollback().await.unwrap();
}
