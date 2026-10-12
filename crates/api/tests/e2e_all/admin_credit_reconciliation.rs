use crate::common::*;
use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

#[tokio::test]
async fn reconcile_requires_admin_write_and_preserves_audited_balances() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let org_id = Uuid::parse_str(&org.id).unwrap();
    for (kind, amount) in [("payment", 100), ("postpay", 200)] {
        add_credits_with_type(
            &server,
            &org.id,
            kind,
            None,
            amount,
            "USD",
            &get_session_id(),
        )
        .await;
    }
    // Synthetic pre-attribution opening position; no production DB edits.
    let client = db.pool().get().await.unwrap();
    client.execute(
        "INSERT INTO organization_balance (organization_id,total_spent,legacy_unattributed_amount) VALUES ($1,80,80) ON CONFLICT (organization_id) DO UPDATE SET total_spent=80, legacy_unattributed_amount=80",
        &[&org_id],
    ).await.unwrap();
    drop(client);
    let path = format!("/v1/admin/organizations/{}/credit-reconciliations", org.id);
    let body = json!({"idempotency_key":"opening", "amounts":{"payment":80}, "reason":"Verified historical payment usage"});
    let anonymous = server.post(&path).json(&body).await;
    assert_eq!(anonymous.status_code(), StatusCode::UNAUTHORIZED);
    for (permission, expected) in [
        ("read_only", StatusCode::FORBIDDEN),
        ("read_write", StatusCode::OK),
    ] {
        let token = server.post("/v1/admin/access-tokens")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({"name":"reconciliation test", "reason":"integration test", "expires_in_hours":24, "permission":permission})).await;
        assert_eq!(token.status_code(), StatusCode::OK);
        let token = token
            .json::<api::models::AdminAccessTokenResponse>()
            .access_token;
        let mut preview = body.clone();
        preview["dry_run"] = json!(true);
        let response = server
            .post(&path)
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&preview)
            .await;
        assert_eq!(response.status_code(), expected);
        if expected == StatusCode::OK {
            let value = response.json::<Value>();
            assert_eq!(value["reconciliation_id"], Value::Null);
            // Before (100 + 200) - 80 = 220; after (20 + 200) - 0 = 220.
            // Reclassification must preserve aggregate available capacity.
            assert_eq!(value["before"]["aggregate_available"], 220);
            assert_eq!(value["after"]["aggregate_available"], 220);
            assert_eq!(value["after"]["credits"]["payment"]["consumed"], 80);
        }
    }
    let call = |payload: Value| {
        server
            .post(&path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&payload)
    };
    for amounts in [
        json!({"unknown":1}),
        json!({"payment":-1}),
        json!({}),
        json!({"payment":i64::MAX,"postpay":1}),
    ] {
        let mut invalid = body.clone();
        invalid["amounts"] = amounts;
        assert_eq!(call(invalid).await.status_code(), StatusCode::BAD_REQUEST);
    }
    let mut too_large = body.clone();
    too_large["amounts"] = json!({"payment":81});
    assert_eq!(call(too_large).await.status_code(), StatusCode::CONFLICT);
    let commit = call(body.clone()).await;
    assert_eq!(commit.status_code(), StatusCode::OK);
    let saved = commit.json::<Value>();
    assert_eq!(saved["dry_run"], false);
    assert!(saved["reconciliation_id"].is_string());
    assert_eq!(call(body.clone()).await.json::<Value>(), saved);
    let mut conflicting = body.clone();
    conflicting["amounts"] = json!({"postpay":80});
    assert_eq!(call(conflicting).await.status_code(), StatusCode::CONFLICT);
    let client = db.pool().get().await.unwrap();
    let row = client.query_one("SELECT created_by, reason FROM organization_credit_reconciliations WHERE organization_id=$1", &[&org_id]).await.unwrap();
    assert_eq!(
        row.get::<_, Uuid>(0),
        Uuid::parse_str(MOCK_USER_ID).unwrap()
    );
    assert_eq!(row.get::<_, String>(1), "Verified historical payment usage");
    let balance = client.query_one("SELECT total_spent, legacy_unattributed_amount FROM organization_balance WHERE organization_id=$1", &[&org_id]).await.unwrap();
    assert_eq!(balance.get::<_, i64>(0), 80);
    assert_eq!(balance.get::<_, i64>(1), 0);
}
