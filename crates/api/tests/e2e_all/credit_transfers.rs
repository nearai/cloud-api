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
    let policy_path = format!("/v1/admin/organizations/{}/policy", sender.id);
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
        .json(&json!({
            "policy": {
                "credentials": {"enabled": true},
                "credits_transfer": {"enabled": true}
            },
            "reason": "Reviewed"
        }))
        .await;

    assert_eq!(denied.status_code(), StatusCode::FORBIDDEN);

    let enabled = server
        .put(&policy_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "policy": {
                "credentials": {"enabled": true},
                "credits_transfer": {"enabled": true}
            },
            "reason": "Reviewed"
        }))
        .await;

    assert_eq!(enabled.status_code(), StatusCode::OK);

    let credential_path = format!("/v1/organizations/{}/credentials", sender.id);
    let credential = server
        .post(&credential_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "name": "Payout worker",
            "permissions": ["credits:read", "credits:transfer"],
            "constraints": {"credits:transfer": {"max_amount_per_transfer": 30, "max_total_amount": 50}},
            "expires_at": chrono::Utc::now() + chrono::Duration::hours(1),
        }))
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

    let capacity = server
        .get(&format!(
            "/v1/organizations/{}/credit/transfer/capacity",
            sender.id
        ))
        .add_header("Authorization", bearer.clone())
        .await;
    assert_eq!(capacity.status_code(), StatusCode::OK);
    assert_eq!(capacity.json::<Value>()["credential_remaining_budget"], 20);

    let managed = server
        .get(&credential_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(managed.status_code(), StatusCode::OK);
    assert_eq!(managed.json::<Value>()["management_enabled"], true);
    assert!(managed.json::<Value>()["credentials"][0]
        .get("token_hash")
        .is_none());
    let denied_management = server
        .get(&credential_path)
        .add_header("Authorization", bearer.clone())
        .await;
    assert_eq!(denied_management.status_code(), StatusCode::FORBIDDEN);

    let reconciliation = server
        .get(&format!(
            "/v1/admin/organizations/{}/credit/transfer/reconciliation",
            sender.id
        ))
        .add_header("Authorization", format!("Bearer {readonly}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(reconciliation.status_code(), StatusCode::OK);
    assert_eq!(reconciliation.json::<Value>()["consistent"], true);

    let disabled_management = server
        .put(&policy_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "policy": {
                "credentials": {"enabled": false},
                "credits_transfer": {"enabled": true}
            },
            "reason": "Stop issuing credentials"
        }))
        .await;
    assert_eq!(disabled_management.status_code(), StatusCode::OK);
    let blocked_rotation = server
        .post(&format!(
            "{credential_path}/{}/rotate",
            credential["id"].as_str().unwrap()
        ))
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(blocked_rotation.status_code(), StatusCode::FORBIDDEN);
    let managed = server
        .get(&credential_path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(managed.status_code(), StatusCode::OK);
    assert_eq!(managed.json::<Value>()["management_enabled"], false);

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

#[tokio::test]
async fn cumulative_program_payouts_apply_credential_limits_to_delta() {
    let (server, _) = setup_test_server_with_config_and_database(|_| {}).await;
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
    let authorization = format!("Bearer {}", get_session_id());
    let path = format!("/v1/organizations/{}/credit/transfer", sender.id);
    let enabled = server
        .put(&format!("/v1/admin/organizations/{}/policy", sender.id))
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "policy": {
                "credentials": {"enabled": true},
                "credits_transfer": {"enabled": true, "max_sends_per_minute": 2}
            },
            "reason": "Program payout test"
        }))
        .await;
    assert_eq!(enabled.status_code(), StatusCode::OK);

    let credential = server.post(&format!("/v1/organizations/{}/credentials", sender.id))
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "name": "Program worker",
            "permissions": ["credits:transfer"],
            "constraints": {"credits:transfer": {"max_amount_per_transfer": 30, "max_total_amount": 50}},
            "expires_at": chrono::Utc::now() + chrono::Duration::hours(1)
        })).await;
    assert_eq!(credential.status_code(), StatusCode::OK);
    let bearer = format!(
        "Bearer {}",
        credential.json::<Value>()["token"].as_str().unwrap()
    );

    for (target, status, delta) in [
        (30, StatusCode::CREATED, 30),
        (50, StatusCode::CREATED, 20),
        (50, StatusCode::OK, 0),
        (20, StatusCode::OK, 0),
    ] {
        let response = server
            .post(&path)
            .add_header("Authorization", bearer.clone())
            .json(&json!({
                "recipient_organization_id": recipient.id,
                "source_credit_type": "payment",
                "payout_program": "monthly-rewards",
                "recipient_cumulative": target
            }))
            .await;

        assert_eq!(response.status_code(), status);
        assert_eq!(response.json::<Value>()["amount"], delta);
    }

    let exceeded = server
        .post(&path)
        .add_header("Authorization", bearer)
        .json(&json!({
            "recipient_organization_id": recipient.id,
            "source_credit_type": "payment",
            "payout_program": "monthly-rewards",
            "recipient_cumulative": 60
        }))
        .await;
    assert_eq!(exceeded.status_code(), StatusCode::CONFLICT);
    assert!(exceeded
        .text()
        .contains("credential_transfer_limit_exceeded"));
}

#[tokio::test]
async fn policy_put_requires_both_sections_without_changing_policy_or_audit() {
    let server = setup_test_server().await;
    let sender = create_org(&server).await;
    let path = format!("/v1/admin/organizations/{}/policy", sender.id);
    let authorization = format!("Bearer {}", get_session_id());

    let initial = server
        .put(&path)
        .add_header("Authorization", authorization.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "policy": {
                "credentials": {"enabled": true},
                "credits_transfer": {"enabled": true}
            },
            "reason": "Enable both sections"
        }))
        .await;

    assert_eq!(initial.status_code(), StatusCode::OK);
    let saved = initial.json::<Value>();

    for policy in [
        json!({"credits_transfer": {"enabled": true}}),
        json!({"credentials": {"enabled": true}}),
    ] {
        let rejected = server
            .put(&path)
            .add_header("Authorization", authorization.clone())
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({
                "policy": policy,
                "reason": "Incomplete replacement"
            }))
            .await;

        assert_eq!(rejected.status_code(), StatusCode::BAD_REQUEST);

        let current = server
            .get(&path)
            .add_header("Authorization", authorization.clone())
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;

        assert_eq!(current.json::<Value>(), saved);
    }
}
