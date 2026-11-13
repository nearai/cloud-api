use crate::common::*;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use database::repositories::credit_transfers::CreditTransferRepository;
use database::repositories::organization_credentials::OrganizationCredentialRepository;
use serde_json::{json, Value};
use services::organization_credits::models::{
    CredentialConstraints, CredentialPolicy, CredentialRequest, OrganizationPolicy,
    TransferConstraints, TransferPolicy, READ_CREDITS, TRANSFER_CREDITS,
};
use std::sync::Arc;
use uuid::Uuid;

/// Saves the organization's policy directly; the credit routes only read it.
async fn save_policy(
    database: &Arc<database::Database>,
    org: &str,
    transfers_enabled: bool,
    max_sends_per_minute: i64,
) {
    let policy = OrganizationPolicy {
        credentials: CredentialPolicy { enabled: true },
        credits_transfer: TransferPolicy {
            enabled: transfers_enabled,
            max_sends_per_minute,
            ..Default::default()
        },
    };
    CreditTransferRepository::new(
        database.pool().clone(),
        &config::CreditAllocationConfig::default(),
    )
    .configure_policy(
        Uuid::parse_str(org).unwrap(),
        Some((policy, Uuid::parse_str(MOCK_USER_ID).unwrap(), "Reviewed")),
    )
    .await
    .unwrap();
}

/// Creates a credential directly and returns its repository, id and bearer token.
async fn create_credential(
    database: &Arc<database::Database>,
    org: &str,
    permissions: &[&str],
) -> (OrganizationCredentialRepository, Uuid, String) {
    let repository = OrganizationCredentialRepository::new(database.pool().clone());
    let org_id = Uuid::parse_str(org).unwrap();
    let credential = repository
        .save(
            org_id,
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            None,
            &CredentialRequest {
                name: "Payout worker".into(),
                permissions: permissions.iter().map(|p| p.to_string()).collect(),
                constraints: CredentialConstraints {
                    transfer: TransferConstraints {
                        max_amount_per_transfer: Some(30),
                        max_total_amount: Some(50),
                        ..Default::default()
                    },
                },
                expires_at: Utc::now() + Duration::hours(1),
            },
        )
        .await
        .unwrap();
    let id = Uuid::parse_str(credential["id"].as_str().unwrap()).unwrap();
    let token = format!("Bearer {}", credential["token"].as_str().unwrap());
    (repository, id, token)
}

#[tokio::test]
async fn transfer_credentials_policy_replays_and_validation() {
    let (server, database) = setup_test_server_with_config_and_database(|_| {}).await;
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

    save_policy(&database, &sender.id, true, 60).await;
    let (credentials, credential_id, bearer) =
        create_credential(&database, &sender.id, &[READ_CREDITS, TRANSFER_CREDITS]).await;
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

    // A rotated secret replaces the old one; a revoked credential is refused.
    let capacity_path = format!("/v1/organizations/{}/credit/transfer/capacity", sender.id);
    let rotated = credentials
        .rotate(
            Uuid::parse_str(&sender.id).unwrap(),
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            credential_id,
        )
        .await
        .unwrap();
    let old_secret = server
        .get(&capacity_path)
        .add_header("Authorization", bearer)
        .await;

    assert_eq!(old_secret.status_code(), StatusCode::FORBIDDEN);

    let bearer = format!("Bearer {}", rotated["token"].as_str().unwrap());
    let rotated_capacity = server
        .get(&capacity_path)
        .add_header("Authorization", bearer.clone())
        .await;

    assert_eq!(rotated_capacity.status_code(), StatusCode::OK);
    assert_eq!(
        rotated_capacity.json::<Value>()["credential_remaining_budget"],
        20
    );

    credentials
        .revoke(
            Uuid::parse_str(&sender.id).unwrap(),
            Uuid::parse_str(MOCK_USER_ID).unwrap(),
            credential_id,
        )
        .await
        .unwrap();

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
    let (server, database) = setup_test_server_with_config_and_database(|_| {}).await;
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
    save_policy(&database, &sender.id, true, 2).await;
    let (_, _, bearer) = create_credential(&database, &sender.id, &[TRANSFER_CREDITS]).await;

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
