use crate::common::*;
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

async fn call(
    server: &axum_test::TestServer,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> axum_test::TestResponse {
    let request = server
        .method(method, path)
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT);
    match body {
        Some(body) => request.json(&body).await,
        None => request.await,
    }
}

fn set(amount: i64, statement: &str) -> Option<Value> {
    Some(json!({"cumulativeNanoUsd": amount, "statementRef": statement}))
}

#[tokio::test]
async fn trace_commons_credit_is_monotonic_and_idempotent() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let path = format!("/v1/admin/organizations/{}/trace-commons/credit", org.id);
    let session = get_session_id();

    let get = call(&server, Method::GET, &path, &session, None).await;
    assert_eq!(get.status_code(), StatusCode::OK);
    assert_eq!(get.json::<Value>()["cumulativeNanoUsd"], 0);

    for (amount, applied, effective) in [
        (100, true, 100),
        // A retry of the same total writes nothing.
        (100, false, 100),
        // An older, smaller total delivered late writes nothing.
        (50, false, 100),
        (250, true, 250),
    ] {
        let response = call(
            &server,
            Method::PUT,
            &path,
            &session,
            set(amount, "sha256:ab"),
        )
        .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{amount}");
        let body = response.json::<Value>();
        assert_eq!(body["applied"], applied, "{amount}");
        assert_eq!(body["cumulativeNanoUsd"], effective, "{amount}");
    }

    let history = call(
        &server,
        Method::GET,
        &format!("/v1/admin/organizations/{}/limits/history", org.id),
        &session,
        None,
    )
    .await
    .json::<Value>();
    let rows: Vec<&Value> = history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["type"] == "trace_commons")
        .collect();
    assert_eq!(rows.len(), 2, "only the two raises are recorded");
    assert_eq!(rows[0]["source"], "trace-commons");
    assert_eq!(rows[0]["changedBy"], "trace_commons_sync");

    let balance = call(
        &server,
        Method::GET,
        &format!("/v1/admin/organizations/{}/usage/balance", org.id),
        &session,
        None,
    )
    .await
    .json::<Value>();
    let entry = balance["credit_limits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["type"] == "trace_commons")
        .cloned()
        .unwrap();
    assert_eq!(entry["amount"], 250);
}

#[tokio::test]
async fn only_the_dedicated_route_writes_trace_commons_credit() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let session = get_session_id();
    let path = format!("/v1/admin/organizations/{}/trace-commons/credit", org.id);
    call(&server, Method::PUT, &path, &session, set(500, "s1")).await;

    let patch = call(
        &server,
        Method::PATCH,
        &format!("/v1/admin/organizations/{}/limits", org.id),
        &session,
        Some(json!({"type": "trace_commons", "spendLimit": {"amount": 1, "currency": "USD"}})),
    )
    .await;
    assert_eq!(patch.status_code(), StatusCode::BAD_REQUEST);

    // A grant write replaces grants only.
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        7,
        "USD",
        &session,
    )
    .await;
    let get = call(&server, Method::GET, &path, &session, None).await;
    assert_eq!(get.json::<Value>()["cumulativeNanoUsd"], 500);
}

#[tokio::test]
async fn trace_commons_credit_rejects_bad_input_and_unknown_orgs() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let session = get_session_id();
    let path = format!("/v1/admin/organizations/{}/trace-commons/credit", org.id);
    for body in [
        set(-1, "s"),
        set(1, ""),
        set(1, "has space"),
        set(1_000_000_000_000_000_001, "s"),
    ] {
        let response = call(&server, Method::PUT, &path, &session, body).await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    }
    let unknown = format!(
        "/v1/admin/organizations/{}/trace-commons/credit",
        Uuid::new_v4()
    );
    assert_eq!(
        call(&server, Method::PUT, &unknown, &session, set(1, "s"))
            .await
            .status_code(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&server, Method::GET, &unknown, &session, None)
            .await
            .status_code(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_read_only_admin_token_reads_but_cannot_set_trace_commons_credit() {
    let (server, _db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let created = call(
        &server,
        Method::POST,
        "/v1/admin/access-tokens",
        &get_session_id(),
        Some(json!({
            "name": format!("tc-read-{}", Uuid::new_v4()),
            "reason": "Trace Commons credit read-only test",
            "expires_in_hours": 1,
            "permission": "read_only"
        })),
    )
    .await;
    assert_eq!(created.status_code(), StatusCode::OK);
    let token = created.json::<Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/v1/admin/organizations/{}/trace-commons/credit", org.id);
    assert_eq!(
        call(&server, Method::GET, &path, &token, None)
            .await
            .status_code(),
        StatusCode::OK
    );
    assert_eq!(
        call(&server, Method::PUT, &path, &token, set(1, "s"))
            .await
            .status_code(),
        StatusCode::FORBIDDEN
    );
}
