use crate::common::*;
use axum::http::StatusCode;
use serde_json::{json, Value};

#[tokio::test]
async fn transfer_credentials_policy_replays_and_validation() {
    let (server, _) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let sender = create_org(&server).await;
    let recipient = create_org(&server).await;
    add_credits_with_type(
        &server,
        &sender.id,
        "payment",
        None,
        100,
        "USD",
        &get_session_id(),
    )
    .await;
    let path = format!("/v1/organizations/{}/credit/transfer", sender.id);
    let policy_path = format!(
        "/v1/admin/organizations/{}/credit/transfer-policy",
        sender.id
    );
    let body = json!({
        "recipient_organization_id": recipient.id,
        "source_credit_type": "payment",
        "amount": 30,
        "memo": "  opaque  ",
        "metadata": {"reference": "hash"},
    });
    let authorization = format!("Bearer {}", get_session_id());

    let disabled = server
        .post(&path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("Idempotency-Key", "order-1")
        .json(&body)
        .await;

    assert_eq!(disabled.status_code(), StatusCode::FORBIDDEN);

    let readonly = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "name": "transfer-readonly-test",
            "reason": "test",
            "expires_in_hours": 1,
            "permission": "read_only",
        }))
        .await;
    let readonly = readonly
        .json::<api::models::AdminAccessTokenResponse>()
        .access_token;
    let denied = server
        .put(&policy_path)
        .add_header("Authorization", format!("Bearer {readonly}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"enabled": true, "reason": "Reviewed"}))
        .await;

    assert_eq!(denied.status_code(), StatusCode::FORBIDDEN);

    let enabled = server
        .put(&policy_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"enabled": true, "reason": "Reviewed"}))
        .await;

    assert_eq!(enabled.status_code(), StatusCode::OK);

    let credential_path = format!(
        "/v1/organizations/{}/credit/transfer-credentials",
        sender.id
    );
    let credential = server
        .post(&credential_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"expires_at": chrono::Utc::now() + chrono::Duration::hours(1)}))
        .await;
    assert_eq!(credential.status_code(), StatusCode::OK);
    let credential = credential.json::<Value>();
    let bearer = format!("Bearer {}", credential["token"].as_str().unwrap());
    let created = server
        .post(&path)
        .add_header("Authorization", bearer.clone())
        .add_header("Idempotency-Key", "order-1")
        .json(&body)
        .await;

    assert_eq!(created.status_code(), StatusCode::CREATED);
    let created = created.json::<Value>();
    assert!(created.get("recipient_snapshot").is_none());
    assert_eq!(created["memo"], "  opaque  ");

    let replay = server
        .post(&path)
        .add_header("Authorization", bearer.clone())
        .add_header("Idempotency-Key", "order-1")
        .json(&body)
        .await;

    assert_eq!(replay.status_code(), StatusCode::OK);
    assert_eq!(replay.json::<Value>(), created);

    for invalid in [Value::Null, json!([]), json!("encoded JSON")] {
        let mut malformed = body.clone();
        malformed["metadata"] = invalid;
        let response = server
            .post(&path)
            .add_header("Authorization", bearer.clone())
            .add_header("Idempotency-Key", "invalid")
            .json(&malformed)
            .await;

        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    }

    let no_key = server
        .post(&path)
        .add_header("Authorization", bearer.clone())
        .json(&body)
        .await;
    assert_eq!(no_key.status_code(), StatusCode::BAD_REQUEST);

    let list = server
        .get(&format!(
            "/v1/organizations/{}/credit/transfers?idempotency_key=order-1",
            sender.id
        ))
        .add_header("Authorization", bearer.clone())
        .await;
    assert_eq!(list.status_code(), StatusCode::OK);
    assert_eq!(list.json::<Value>()["transfers"][0]["id"], created["id"]);

    let revoked = server
        .delete(&format!(
            "{credential_path}/{}",
            credential["id"].as_str().unwrap()
        ))
        .add_header("Authorization", authorization)
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(revoked.status_code(), StatusCode::OK);

    let refused = server
        .post(&path)
        .add_header("Authorization", bearer)
        .add_header("Idempotency-Key", "order-1")
        .json(&body)
        .await;
    assert_eq!(refused.status_code(), StatusCode::FORBIDDEN);
}
