//! Canonical discounts must leave billing, balances, allocation reports and hourly
//! snapshots in agreement. This module owns the global hourly lock and runs serially.
use crate::common::*;
use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use database::{
    models::RecordUsageRequest,
    repositories::{
        organization_usage_discount::OrganizationUsageDiscountRepository,
        OrganizationUsageRepository,
    },
};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

const INTERNAL_TOKEN: &str = "synthetic-org-discount-reporter";
const BASIS_POINTS: i32 = 2500;

struct Fixture {
    server: axum_test::TestServer,
    db: Arc<database::Database>,
    org: Uuid,
    workspace: Uuid,
    key: Uuid,
    key_secret: String,
    model: Uuid,
    model_name: String,
}

async fn fixture() -> Fixture {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.internal_usage_token = Some(INTERNAL_TOKEN.into());
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id.clone()).await[0].id.clone();
    let key =
        create_api_key_in_workspace(&server, workspace.clone(), "discount fixture".into()).await;
    let model_name = format!("synthetic/discount-{}", Uuid::new_v4());
    let model = db
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "INSERT INTO models (model_name, model_display_name, model_description,
          input_cost_per_token, output_cost_per_token, context_length, max_output_length,
          verifiable, is_active, provider_type, attestation_supported)
         VALUES ($1, $1, 'Synthetic discount fixture', 1000000, 2000000, 4096, 1024,
                 false, true, 'external', false) RETURNING id",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);
    Fixture {
        server,
        db,
        org: Uuid::parse_str(&org.id).unwrap(),
        workspace: Uuid::parse_str(&workspace).unwrap(),
        key: Uuid::parse_str(&key.id).unwrap(),
        key_secret: key.key.unwrap(),
        model,
        model_name,
    }
}

async fn admin(f: &Fixture, method: Method, token: &str, body: Value) -> axum_test::TestResponse {
    f.server
        .method(
            method,
            &format!("/v1/admin/organizations/{}/usage-discount", f.org),
        )
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn save(f: &Fixture, since: Option<DateTime<Utc>>) -> Value {
    let response = admin(
        f,
        Method::PUT,
        &get_session_id(),
        json!({
            "discount_basis_points": BASIS_POINTS, "apply_since": since,
        }),
    )
    .await;
    assert_eq!(
        response.status_code(),
        if since.is_some() {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        "{}",
        response.text()
    );
    response.json()
}

async fn get(f: &Fixture, path: &str) -> Value {
    let response = f
        .server
        .get(path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        StatusCode::OK,
        "{}",
        response.text()
    );
    response.json()
}

async fn credits(f: &Fixture, kind: &str, amount: i64) {
    add_credits_with_type(
        &f.server,
        &f.org.to_string(),
        kind,
        None,
        amount,
        "USD",
        &get_session_id(),
    )
    .await;
}

fn request(f: &Fixture, cost: i64) -> RecordUsageRequest {
    let inference = Uuid::new_v4();
    RecordUsageRequest {
        organization_id: f.org,
        workspace_id: f.workspace,
        api_key_id: f.key,
        model_id: f.model,
        model_name: f.model_name.clone(),
        input_tokens: 100,
        output_tokens: 50,
        input_cost: cost / 2,
        output_cost: cost - cost / 2,
        total_cost: cost,
        inference_type: "chat_completion".into(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(inference),
        provider_request_id: Some(format!("synthetic-{inference}")),
        stop_reason: None,
        response_id: None,
        image_count: None,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        billing_details: None,
        service_tier: None,
        context_band: None,
        served_provider_tier: None,
        served_provider_type: None,
        served_via_fallback: false,
    }
}

/// Legacy rows deliberately bypass today's allocator; all other fixtures use the
/// actual posting path or explicitly seed a balanced historical ledger.
async fn legacy(f: &Fixture, at: DateTime<Utc>, request: &RecordUsageRequest) -> Uuid {
    f.db.pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "INSERT INTO organization_usage_log
         (organization_id, workspace_id, api_key_id, model_id, model_name,
          input_tokens, output_tokens, total_tokens, input_cost, output_cost, total_cost,
          inference_type, inference_id, provider_request_id, created_at)
         VALUES ($1,$2,$3,$4,$5,100,50,150,$6,$7,$8,'chat_completion',$9,$10,$11) RETURNING id",
            &[
                &f.org,
                &f.workspace,
                &f.key,
                &f.model,
                &f.model_name,
                &request.input_cost,
                &request.output_cost,
                &request.total_cost,
                &request.inference_id,
                &request.provider_request_id,
                &at,
            ],
        )
        .await
        .unwrap()
        .get(0)
}

async fn seed_balance(f: &Fixture, legacy: i64) {
    f.db.pool().get().await.unwrap().execute(
        "INSERT INTO organization_balance (organization_id,total_spent,total_requests,total_tokens,
          legacy_unattributed_amount,updated_at)
         SELECT $1,SUM(total_cost)::BIGINT,COUNT(*),SUM(total_tokens)::BIGINT,$2,NOW()
         FROM organization_usage_log WHERE organization_id=$1
         ON CONFLICT (organization_id) DO UPDATE SET total_spent=EXCLUDED.total_spent,
           total_requests=EXCLUDED.total_requests,total_tokens=EXCLUDED.total_tokens,
           legacy_unattributed_amount=EXCLUDED.legacy_unattributed_amount",
        &[&f.org, &legacy],
    ).await.unwrap();
}

async fn finish(f: &Fixture) {
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    for _ in 0..5 {
        if repo.get(f.org).await.unwrap().unwrap().status == "active" {
            return;
        }
        assert!(repo.apply_batch(f.org).await.unwrap());
    }
    panic!("small synthetic history did not finish");
}

#[tokio::test]
async fn admin_permissions_validation_and_identical_save_retry() {
    let f = fixture().await;
    f.server
        .get(&format!("/v1/admin/organizations/{}/usage-discount", f.org))
        .await
        .assert_status_unauthorized();
    admin(&f, Method::GET, &get_session_id(), json!({}))
        .await
        .assert_json(&Value::Null);
    for body in [
        json!({"discount_basis_points":0}),
        json!({"discount_basis_points":10001}),
        json!({"discount_basis_points":-1}),
        json!({"discount_basis_points":2500,"apply_since":Utc::now()+Duration::days(1)}),
    ] {
        admin(&f, Method::PUT, &get_session_id(), body)
            .await
            .assert_status_bad_request();
    }
    for body in [
        json!({"discount_basis_points":1.5}),
        json!({"discount_basis_points":"2500"}),
        json!({"discount_basis_points":2500,"apply_since":"invalid"}),
        json!({}),
    ] {
        assert!(admin(&f, Method::PUT, &get_session_id(), body)
            .await
            .status_code()
            .is_client_error());
    }
    let issued = f.server.post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}",get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"name":"Synthetic discount read test","reason":"test","expires_in_hours":1,"permission":"read_only"})).await;
    issued.assert_status_ok();
    let read_token = issued
        .json::<api::models::AdminAccessTokenResponse>()
        .access_token;
    admin(&f, Method::GET, &read_token, json!({}))
        .await
        .assert_status_ok();
    admin(
        &f,
        Method::PUT,
        &read_token,
        json!({"discount_basis_points":BASIS_POINTS}),
    )
    .await
    .assert_status_forbidden();
    admin(&f, Method::GET, &get_session_id(), json!({}))
        .await
        .assert_json(&Value::Null);

    // A real authenticated organization owner is not a platform administrator.
    let (session, _) = setup_unique_test_session(&f.db).await;
    let user = Uuid::parse_str(session.trim_start_matches("rt_")).unwrap();
    let client = f.db.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET email=$2 WHERE id=$1",
            &[&user, &format!("{user}@example.org")],
        )
        .await
        .unwrap();
    client.execute("INSERT INTO organization_members (organization_id,user_id,role,joined_at) VALUES ($1,$2,'owner',NOW())", &[&f.org,&user]).await.unwrap();
    drop(client);
    let (real_server, _) =
        setup_test_server_with_config_and_database(|config| config.auth.mock = false).await;
    let (_, refresh) = database::repositories::SessionRepository::new(f.db.pool().clone())
        .create(user, None, MOCK_USER_AGENT.into(), 1)
        .await
        .unwrap();
    let token = get_access_token_from_refresh_token(&real_server, refresh).await;
    for method in [Method::GET, Method::PUT] {
        let response = real_server
            .method(
                method,
                &format!("/v1/admin/organizations/{}/usage-discount", f.org),
            )
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({"discount_basis_points":BASIS_POINTS}))
            .await;
        assert!(matches!(
            response.status_code(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
    }
    let saved = save(&f, None).await;
    assert_eq!(saved["status"], "active");
    assert_eq!(save(&f, None).await, saved);
    admin(
        &f,
        Method::PUT,
        &get_session_id(),
        json!({"discount_basis_points":3000}),
    )
    .await
    .assert_status(StatusCode::CONFLICT);
    admin(
        &f,
        Method::PUT,
        &get_session_id(),
        json!({"discount_basis_points":BASIS_POINTS,"apply_since":Utc::now()-Duration::days(1)}),
    )
    .await
    .assert_status(StatusCode::CONFLICT);
    assert_eq!(
        admin(&f, Method::GET, &read_token, json!({}))
            .await
            .json::<Value>(),
        saved
    );
}

async fn report(f: &Fixture, id: &str, input_tokens: i32) -> axum_test::TestResponse {
    f.server
        .post("/v1/internal/usage")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .json(
            &json!({"organization_id":f.org,"workspace_id":f.workspace,"api_key_id":f.key,
            "type":"chat_completion","model":f.model_name,"id":id,
            "input_tokens":input_tokens,"output_tokens":50,"discount_to_user":0.2}),
        )
        .await
}

#[tokio::test]
async fn forward_discount_composes_with_reporter_price_and_retries_once() {
    let f = fixture().await;
    credits(&f, "grant", 2_000_000_000).await;
    let old = report(&f, "synthetic-before-rule", 100).await;
    old.assert_status_ok();
    let old: Value = old.json();
    assert_eq!(old["total_cost"], 160_000_000);
    save(&f, None).await;
    let retry = report(&f, "synthetic-before-rule", 100).await;
    retry.assert_status_ok();
    assert_eq!(retry.json::<Value>()["total_cost"], 160_000_000);
    assert_eq!(retry.json::<Value>()["id"], old["id"]);

    let fresh = report(&f, "synthetic-after-rule", 100).await;
    fresh.assert_status_ok();
    let fresh: Value = fresh.json();
    assert_eq!(fresh["input_cost"], 60_000_000);
    assert_eq!(fresh["output_cost"], 60_000_000);
    assert_eq!(fresh["total_cost"], 120_000_000);
    let retry = report(&f, "synthetic-after-rule", 100).await;
    retry.assert_status_ok();
    assert_eq!(retry.json::<Value>()["id"], fresh["id"]);
    assert_eq!(retry.json::<Value>()["total_cost"], 120_000_000);
    assert!(!report(&f, "synthetic-after-rule", 101)
        .await
        .status_code()
        .is_success());
    let history = get(&f, &format!("/v1/organizations/{}/usage/history", f.org)).await;
    let row = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == fresh["id"])
        .unwrap();
    assert_eq!(
        row["billingDetails"]["discount"]["list_total_cost"],
        200_000_000
    );
    assert_eq!(
        row["billingDetails"]["contract_discount"]["total_cost"],
        160_000_000
    );
    assert_eq!(row["funded_amount"], 120_000_000);
    assert_eq!(
        get(&f, &format!("/v1/organizations/{}/usage/balance", f.org)).await["total_spent"],
        280_000_000
    );
    let client = f.db.pool().get().await.unwrap();
    let totals = client.query_one("SELECT COUNT(*) AS count,SUM(amount)::BIGINT AS amount FROM usage_credit_allocations WHERE organization_id=$1", &[&f.org]).await.unwrap();
    assert_eq!(totals.get::<_, i64>("count"), 2);
    assert_eq!(totals.get::<_, i64>("amount"), 280_000_000);
}

#[tokio::test]
async fn history_corrects_legacy_and_split_funding_with_reporting_parity() {
    let f = fixture().await;
    credits(&f, "grant", 300_000_000).await;
    credits(&f, "postpay", 900_000_000).await;
    let hour = crate::usage_hourly::random_past_hour();
    let before = request(&f, 100_000_000);
    legacy(&f, hour - Duration::microseconds(1), &before).await;
    let old = request(&f, 100_000_000);
    let legacy_id = legacy(&f, hour, &old).await;
    let split = request(&f, 200_000_000);
    let split_id = legacy(&f, hour + Duration::minutes(1), &split).await;
    let paid = request(&f, 200_000_000);
    let paid_id = legacy(&f, hour + Duration::minutes(2), &paid).await;
    let client = f.db.pool().get().await.unwrap();
    for (id, kind, amount, priority) in [
        (split_id, "grant", 150_000_000_i64, 0_i32),
        (split_id, "postpay", 50_000_000, 1),
        (paid_id, "postpay", 200_000_000, 1),
    ] {
        client.execute("INSERT INTO usage_credit_allocations (organization_id,inference_usage_id,credit_type,amount,organization_limit_id,policy_version,priority_position)
         SELECT $1,$2,$3::VARCHAR,$4,id,'synthetic', $5::INTEGER FROM organization_limits_history WHERE organization_id=$1 AND credit_type=$3::VARCHAR AND effective_until IS NULL", &[&f.org,&id,&kind,&amount,&priority]).await.unwrap();
    }
    client.execute("UPDATE organization_usage_log SET funded_amount=total_cost,unfunded_amount=0,allocation_policy_version='synthetic' WHERE id=ANY($1)",&[&vec![split_id,paid_id]]).await.unwrap();
    for (kind, amount) in [("grant", 150_000_000_i64), ("postpay", 250_000_000_i64)] {
        client.execute("INSERT INTO organization_credit_consumption (organization_id,credit_type,amount) VALUES ($1,$2,$3)",&[&f.org,&kind,&amount]).await.unwrap();
    }
    drop(client);
    seed_balance(&f, 200_000_000).await;
    crate::usage_hourly::recompute_usage_hours(
        hour - Duration::hours(1),
        hour + Duration::hours(1),
    )
    .await;
    save(&f, Some(hour)).await;
    let usage = OrganizationUsageRepository::new(f.db.pool().clone());
    let old_retry = usage.record_usage(old.clone()).await.unwrap();
    assert!(!old_retry.was_inserted);
    assert_eq!(old_retry.total_cost, 100_000_000);
    finish(&f).await;
    let corrected_retry = usage.record_usage(old.clone()).await.unwrap();
    assert!(!corrected_retry.was_inserted);
    assert_eq!(corrected_retry.total_cost, 75_000_000);
    assert_eq!(
        usage.record_usage(before).await.unwrap().total_cost,
        100_000_000
    );
    let mut changed = old;
    changed.input_cost += 1;
    changed.total_cost += 1;
    assert!(
        usage.record_usage(changed).await.is_err(),
        "changed original payload must not become a retry after rounding"
    );
    assert_eq!(usage.get_api_key_spend(f.key).await.unwrap(), 475_000_000);

    let client = f.db.pool().get().await.unwrap();
    let audit = client.query_one("SELECT COUNT(*),SUM(original_total_cost)::BIGINT FROM usage_discount_adjustments WHERE usage_id=ANY($1)",&[&vec![legacy_id,split_id,paid_id]]).await.unwrap();
    assert_eq!(
        (audit.get::<_, i64>(0), audit.get::<_, i64>(1)),
        (3, 500_000_000)
    );
    let allocations = client
        .query_one(
            "SELECT SUM(amount)::BIGINT FROM usage_credit_allocations WHERE organization_id=$1",
            &[&f.org],
        )
        .await
        .unwrap();
    assert_eq!(
        allocations.get::<_, i64>(0),
        400_000_000,
        "original allocations remain immutable"
    );
    let balances = client.query_one("SELECT total_spent,legacy_unattributed_amount,total_requests,total_tokens FROM organization_balance WHERE organization_id=$1", &[&f.org]).await.unwrap();
    assert_eq!(
        (
            balances.get::<_, i64>(0),
            balances.get::<_, i64>(1),
            balances.get::<_, i64>(2),
            balances.get::<_, i64>(3)
        ),
        (475_000_000, 175_000_000, 4, 600)
    );
    let hourly = client.query_one("SELECT SUM(total_cost)::BIGINT,SUM(request_count)::BIGINT FROM usage_hourly WHERE organization_id=$1", &[&f.org]).await.unwrap();
    assert_eq!(
        (hourly.get::<_, i64>(0), hourly.get::<_, i64>(1)),
        (475_000_000, 4)
    );
    let split_funding = client.query("SELECT credit_type,amount FROM effective_usage_credit_allocations WHERE inference_usage_id=$1 ORDER BY credit_type", &[&split_id]).await.unwrap();
    assert_eq!(split_funding.len(), 1);
    assert_eq!(
        (
            split_funding[0].get::<_, String>(0),
            split_funding[0].get::<_, i64>(1)
        ),
        ("grant".into(), 150_000_000)
    );
    drop(client);
    for prefix in ["/v1/organizations", "/v1/admin/organizations"] {
        let balance = get(&f, &format!("{prefix}/{}/usage/balance", f.org)).await;
        assert_eq!(balance["total_spent"], 475_000_000);
        assert_eq!(balance["total_requests"], 4);
        assert_eq!(balance["remaining"], 725_000_000);
    }
    let billing = f
        .server
        .post("/v1/billing/costs")
        .add_header("Authorization", format!("Bearer {}", f.key_secret))
        .json(&json!({"requestIds":[split.inference_id,paid.inference_id]}))
        .await;
    billing.assert_status_ok();
    let billed: Value = billing.json();
    assert_eq!(
        billed["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["costNanoUsd"].as_i64().unwrap())
            .sum::<i64>(),
        300_000_000
    );
    let range = format!(
        "start={}&end={}",
        hour.format("%Y-%m-%dT%H:%M:%SZ"),
        (hour + Duration::hours(1)).format("%Y-%m-%dT%H:%M:%SZ")
    );
    for (filter, cost, count) in [
        ("", 0.375, 3),
        ("&credit_type=grant", 0.15, 1),
        ("&credit_type=postpay", 0.15, 1),
    ] {
        let metrics = get(
            &f,
            &format!("/v1/admin/organizations/{}/metrics?{range}{filter}", f.org),
        )
        .await;
        assert_eq!(metrics["summary"]["total_cost_usd"], cost);
        assert_eq!(metrics["summary"]["total_requests"], count);
        let points = get(
            &f,
            &format!(
                "/v1/admin/organizations/{}/metrics/timeseries?{range}{filter}&granularity=hour",
                f.org
            ),
        )
        .await;
        assert_eq!(
            points["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["cost_usd"].as_f64().unwrap())
                .sum::<f64>(),
            cost
        );
    }
    assert_eq!(
        get(
            &f,
            &format!("/v1/admin/organizations/{}/usage-discount", f.org)
        )
        .await["processed_count"],
        3
    );
}

#[tokio::test]
async fn historical_batches_rollback_resume_and_never_compound() {
    let f = fixture().await;
    let hour = crate::usage_hourly::random_past_hour();
    let client = f.db.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO organization_usage_log
        (organization_id,workspace_id,api_key_id,model_id,model_name,input_tokens,output_tokens,
         total_tokens,input_cost,output_cost,total_cost,inference_type,created_at)
        SELECT $1,$2,$3,$4,$5,1,1,2,2,4,6,'chat_completion',$6 FROM generate_series(1,503)",
            &[&f.org, &f.workspace, &f.key, &f.model, &f.model_name, &hour],
        )
        .await
        .unwrap();
    drop(client);
    seed_balance(&f, 3018).await;
    crate::usage_hourly::recompute_usage_hours(hour, hour + Duration::hours(1)).await;
    save(&f, Some(hour)).await;
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    let mut lock_client = f.db.pool().get().await.unwrap();
    let lock = lock_client.transaction().await.unwrap();
    lock.query_one(
        "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
        &[&f.org],
    )
    .await
    .unwrap();
    assert!(
        !repo.apply_batch(f.org).await.unwrap(),
        "busy org is skipped without consuming the cursor"
    );
    lock.rollback().await.unwrap();
    drop(lock_client);
    // Simulate a reconciliation failure after work has begun inside the transaction.
    let client = f.db.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organization_balance SET legacy_unattributed_amount=0 WHERE organization_id=$1",
            &[&f.org],
        )
        .await
        .unwrap();
    drop(client);
    assert!(repo.apply_batch(f.org).await.is_err());
    let client = f.db.pool().get().await.unwrap();
    let rollback = client.query_one("SELECT (SELECT COUNT(*) FROM usage_discount_adjustments j JOIN organization_usage_log u ON u.id=j.usage_id WHERE u.organization_id=$1),
        (SELECT SUM(total_cost)::BIGINT FROM organization_usage_log WHERE organization_id=$1),
        (SELECT SUM(total_cost)::BIGINT FROM usage_hourly WHERE organization_id=$1)", &[&f.org]).await.unwrap();
    assert_eq!(
        (
            rollback.get::<_, i64>(0),
            rollback.get::<_, i64>(1),
            rollback.get::<_, i64>(2)
        ),
        (0, 3018, 3018)
    );
    assert_eq!(repo.get(f.org).await.unwrap().unwrap().processed_count, 0);
    client.execute("UPDATE organization_balance SET legacy_unattributed_amount=3018 WHERE organization_id=$1", &[&f.org]).await.unwrap();
    drop(client);
    assert!(repo.apply_batch(f.org).await.unwrap());
    let progress = repo.get(f.org).await.unwrap().unwrap();
    assert_eq!(
        (progress.status.as_str(), progress.processed_count),
        ("applying", 500)
    );
    // Half-up rounding on each component: 2 * .75 -> 2; 4 * .75 -> 3.
    assert_eq!(
        get(
            &f,
            &format!("/v1/admin/organizations/{}/usage/balance", f.org)
        )
        .await["total_spent"],
        2518
    );
    // A fresh repository represents worker restart; the persisted composite cursor
    // must distinguish the 503 rows even though every timestamp is identical.
    let restarted = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    assert!(restarted.apply_batch(f.org).await.unwrap());
    finish(&f).await;
    assert!(!restarted.apply_batch(f.org).await.unwrap());
    let final_rule = restarted.get(f.org).await.unwrap().unwrap();
    assert_eq!(final_rule.processed_count, 503);
    let retried = admin(
        &f,
        Method::PUT,
        &get_session_id(),
        json!({"discount_basis_points":BASIS_POINTS,"apply_since":hour}),
    )
    .await;
    retried.assert_status_ok();
    let client = f.db.pool().get().await.unwrap();
    let totals = client.query_one("SELECT b.total_spent,b.legacy_unattributed_amount,
        (SELECT SUM(total_cost)::BIGINT FROM usage_hourly WHERE organization_id=$1),
        (SELECT COUNT(*) FROM usage_discount_adjustments j JOIN organization_usage_log u ON u.id=j.usage_id WHERE u.organization_id=$1)
        FROM organization_balance b WHERE organization_id=$1", &[&f.org]).await.unwrap();
    assert_eq!(
        (
            totals.get::<_, i64>(0),
            totals.get::<_, i64>(1),
            totals.get::<_, i64>(2),
            totals.get::<_, i64>(3)
        ),
        (2515, 2515, 2515, 503)
    );
}

#[tokio::test]
async fn unsupported_history_rejects_before_persisting_terms() {
    for settled in [false, true] {
        let f = fixture().await;
        let hour = crate::usage_hourly::random_past_hour();
        let usage = OrganizationUsageRepository::new(f.db.pool().clone());
        // No capacity means an actual posting creates unfunded usage.
        let row = usage.record_usage(request(&f, 200)).await.unwrap();
        assert_eq!(row.unfunded_amount, Some(200));
        let client = f.db.pool().get().await.unwrap();
        client
            .execute(
                "UPDATE organization_usage_log SET created_at=$2 WHERE id=$1",
                &[&row.id, &hour],
            )
            .await
            .unwrap();
        drop(client);
        if settled {
            // Adding capacity invokes the real settlement path, preserving its
            // overage_settlement provenance rather than faking a posting entry.
            credits(&f, "grant", 400).await;
            let client = f.db.pool().get().await.unwrap();
            let count: i64 = client.query_one("SELECT COUNT(*) FROM usage_credit_allocations WHERE inference_usage_id=$1 AND allocation_phase='overage_settlement'", &[&row.id]).await.unwrap().get(0);
            assert_eq!(count, 1);
        }
        let response = admin(
            &f,
            Method::PUT,
            &get_session_id(),
            json!({"discount_basis_points":BASIS_POINTS,"apply_since":hour}),
        )
        .await;
        response.assert_status(StatusCode::CONFLICT);
        admin(&f, Method::GET, &get_session_id(), json!({}))
            .await
            .assert_json(&Value::Null);
        let stored = usage.get_balance(f.org).await.unwrap().unwrap();
        assert_eq!(stored.total_spent, 200);
    }
}

#[tokio::test]
async fn platform_services_stay_outside_forward_and_historical_discount() {
    use database::repositories::organization_service_usage::{
        OrganizationServiceUsageRepository, RecordServiceUsageRequest,
    };
    let f = fixture().await;
    credits(&f, "grant", 1000).await;
    let client = f.db.pool().get().await.unwrap();
    let service: Uuid = client
        .query_one(
            "INSERT INTO services (service_name,display_name,unit,cost_per_unit)
         VALUES ($1,'Synthetic service','request',80) RETURNING id",
            &[&format!("synthetic-discount-service-{}", Uuid::new_v4())],
        )
        .await
        .unwrap()
        .get(0);
    drop(client);
    let services = OrganizationServiceUsageRepository::new(f.db.pool().clone());
    let mut service_request = RecordServiceUsageRequest {
        organization_id: f.org,
        workspace_id: f.workspace,
        api_key_id: f.key,
        service_id: service,
        quantity: 1,
        total_cost: 80,
        inference_id: Some(Uuid::new_v4()),
    };
    let service_usage = services.record_usage(&service_request).await.unwrap();
    let inference = OrganizationUsageRepository::new(f.db.pool().clone());
    let mut original = request(&f, 6);
    original.input_cost = 2;
    original.output_cost = 4;
    let row = inference.record_usage(original.clone()).await.unwrap();
    let hour = crate::usage_hourly::random_past_hour();
    let client = f.db.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE organization_usage_log SET created_at=$2 WHERE id=$1",
            &[&row.id, &hour],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE organization_service_usage_log SET created_at=$2 WHERE id=$1",
            &[&service_usage.id, &hour],
        )
        .await
        .unwrap();
    drop(client);
    crate::usage_hourly::recompute_usage_hours(hour, hour + Duration::hours(1)).await;
    save(&f, Some(hour)).await;
    finish(&f).await;
    assert_eq!(
        inference
            .record_usage(original.clone())
            .await
            .unwrap()
            .total_cost,
        5
    );
    // Both inputs round to the same discounted cost, but the original billable
    // payload changed, so this must still be rejected as an incompatible retry.
    let mut changed = original;
    changed.input_cost = 3;
    changed.total_cost = 7;
    assert!(inference.record_usage(changed).await.is_err());
    let unchanged = services.record_usage(&service_request).await.unwrap();
    assert_eq!(unchanged.total_cost, 80);
    service_request.inference_id = Some(Uuid::new_v4());
    assert_eq!(
        services
            .record_usage(&service_request)
            .await
            .unwrap()
            .total_cost,
        80
    );
    assert_eq!(
        get(
            &f,
            &format!("/v1/admin/organizations/{}/usage/balance", f.org)
        )
        .await["total_spent"],
        165
    );
    assert_eq!(
        get(
            &f,
            &format!("/v1/admin/organizations/{}/usage-discount", f.org)
        )
        .await["processed_count"],
        1
    );
}

#[tokio::test]
async fn full_discount_zeroes_funding_and_nanosecond_cutoff_retries_are_idempotent() {
    let f = fixture().await;
    credits(&f, "grant", 200).await;
    let usage = OrganizationUsageRepository::new(f.db.pool().clone());
    let original = request(&f, 100);
    let row = usage.record_usage(original.clone()).await.unwrap();
    let hour = crate::usage_hourly::random_past_hour();
    let at = hour + Duration::minutes(1);
    f.db.pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE organization_usage_log SET created_at=$2 WHERE id=$1",
            &[&row.id, &at],
        )
        .await
        .unwrap();
    crate::usage_hourly::recompute_usage_hours(hour, hour + Duration::hours(1)).await;
    let since = hour + Duration::nanoseconds(123_456_789);
    let body = json!({"discount_basis_points":10_000,"apply_since":since});
    let saved = admin(&f, Method::PUT, &get_session_id(), body.clone()).await;
    saved.assert_status(StatusCode::ACCEPTED);
    let saved: Value = saved.json();
    let retry = admin(&f, Method::PUT, &get_session_id(), body.clone()).await;
    retry.assert_status(StatusCode::ACCEPTED);
    assert_eq!(retry.json::<Value>(), saved);
    finish(&f).await;
    let corrected = usage.record_usage(original).await.unwrap();
    assert_eq!(
        (
            corrected.total_cost,
            corrected.funded_amount,
            corrected.unfunded_amount
        ),
        (0, Some(0), Some(0))
    );
    assert!(corrected.credit_allocations.unwrap().is_empty());
    let forward = usage.record_usage(request(&f, 100)).await.unwrap();
    assert_eq!((forward.total_cost, forward.funded_amount), (0, Some(0)));
    let client = f.db.pool().get().await.unwrap();
    let ledger = client.query_one(
        "SELECT (SELECT SUM(amount)::BIGINT FROM usage_credit_allocations WHERE organization_id=$1),
                (SELECT COUNT(*) FROM effective_usage_credit_allocations WHERE organization_id=$1),
                (SELECT amount FROM organization_credit_consumption WHERE organization_id=$1 AND credit_type='grant'),
                (SELECT SUM(total_cost)::BIGINT FROM usage_hourly WHERE organization_id=$1)", &[&f.org],
    ).await.unwrap();
    assert_eq!(
        (
            ledger.get::<_, i64>(0),
            ledger.get::<_, i64>(1),
            ledger.get::<_, i64>(2),
            ledger.get::<_, i64>(3)
        ),
        (100, 0, 0, 0)
    );
    drop(client);
    let balance = get(
        &f,
        &format!("/v1/admin/organizations/{}/usage/balance", f.org),
    )
    .await;
    assert_eq!(balance["total_spent"], 0);
    assert_eq!(balance["remaining"], 200);
    assert_eq!(balance["total_requests"], 2);
    admin(&f, Method::PUT, &get_session_id(), body)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn worker_claims_recover_after_crash_and_fence_stale_workers() {
    let f = fixture().await;
    save(&f, Some(Utc::now() - Duration::days(1))).await;
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    let (left, right) = tokio::join!(repo.claim_next(), repo.claim_next());
    let mut claims: Vec<_> = [left.unwrap(), right.unwrap()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        claims.len(),
        1,
        "concurrent replicas must claim an organization once"
    );
    let claim = claims.pop().unwrap();
    assert_eq!(claim.organization_id, f.org);
    assert!(repo.claim_next().await.unwrap().is_none());
    let client = f.db.pool().get().await.unwrap();
    client.execute("UPDATE organization_usage_discounts SET lease_until=clock_timestamp()-INTERVAL '1 second' WHERE organization_id=$1", &[&f.org]).await.unwrap();
    drop(client);
    let replacement = repo.claim_next().await.unwrap().unwrap();
    assert_eq!(replacement.organization_id, f.org);
    assert!(!repo.run_claimed(claim).await.unwrap());
    assert!(
        repo.claim_next().await.unwrap().is_none(),
        "stale worker must not clear replacement lease"
    );

    // Verification has an independent lock: even an expired live process cannot
    // overlap the replacement's expensive full-history scan.
    let mut client = f.db.pool().get().await.unwrap();
    let tx = client.transaction().await.unwrap();
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::UUID::TEXT,$2))",
        &[&f.org, &0x555344495343_i64],
    )
    .await
    .unwrap();
    assert!(!repo.run_claimed(replacement).await.unwrap());
    tx.rollback().await.unwrap();
    let state = client.query_one("SELECT attempts,worker_token,last_error FROM organization_usage_discounts WHERE organization_id=$1", &[&f.org]).await.unwrap();
    assert_eq!(state.get::<_, i32>("attempts"), 0);
    assert_eq!(state.get::<_, Option<Uuid>>("worker_token"), None);
    assert_eq!(state.get::<_, Option<String>>("last_error"), None);
    client.execute("UPDATE organization_usage_discounts SET next_attempt_at=clock_timestamp() WHERE organization_id=$1", &[&f.org]).await.unwrap();
    drop(client);
    let restarted = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    assert!(restarted
        .run_claimed(restarted.claim_next().await.unwrap().unwrap())
        .await
        .unwrap());
    assert_eq!(
        restarted.get(f.org).await.unwrap().unwrap().status,
        "active"
    );
}

#[tokio::test]
async fn worker_failures_back_off_show_safe_diagnostics_and_clear_after_repair() {
    let f = fixture().await;
    // Synthetic pre-existing drift, outside the selected history.
    f.db.pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE organization_balance SET total_spent=1 WHERE organization_id=$1",
            &[&f.org],
        )
        .await
        .unwrap();
    save(&f, Some(Utc::now() - Duration::days(1))).await;
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    for (attempts, expected_delay) in [(0_i32, 30_i64), (1, 60), (16, 1800)] {
        let client = f.db.pool().get().await.unwrap();
        client.execute("UPDATE organization_usage_discounts SET attempts=$2,next_attempt_at=clock_timestamp() WHERE organization_id=$1", &[&f.org,&attempts]).await.unwrap();
        drop(client);
        let claim = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(claim.organization_id, f.org);
        assert!(repo.run_claimed(claim).await.is_err());
        let response = get(
            &f,
            &format!("/v1/admin/organizations/{}/usage-discount", f.org),
        )
        .await;
        assert_eq!(response["status"], "applying");
        assert_eq!(
            response["last_error"],
            "Accounting verification failed. Reconcile organization balances before retry."
        );
        let retry = DateTime::parse_from_rfc3339(response["next_retry_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        let delay = (retry - Utc::now()).num_seconds();
        assert!(
            (expected_delay - 5..=expected_delay).contains(&delay),
            "{delay}"
        );
        assert!(
            repo.claim_next().await.unwrap().is_none(),
            "backoff must prevent another full scan"
        );
    }
    let client = f.db.pool().get().await.unwrap();
    client.execute("UPDATE organization_balance SET total_spent=0,legacy_unattributed_amount=0 WHERE organization_id=$1", &[&f.org]).await.unwrap();
    client.execute("UPDATE organization_usage_discounts SET next_attempt_at=clock_timestamp() WHERE organization_id=$1", &[&f.org]).await.unwrap();
    drop(client);
    assert!(repo
        .run_claimed(repo.claim_next().await.unwrap().unwrap())
        .await
        .unwrap());
    let response = get(
        &f,
        &format!("/v1/admin/organizations/{}/usage-discount", f.org),
    )
    .await;
    assert_eq!(response["status"], "active");
    assert_eq!(response["last_error"], Value::Null);
    assert_eq!(response["next_retry_at"], Value::Null);
}

#[tokio::test]
async fn worker_queue_reaches_newer_orgs_behind_more_than_32_failures() {
    let f = fixture().await;
    let client = f.db.pool().get().await.unwrap();
    let user = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let rows = client.query("WITH org AS (INSERT INTO organizations (name) SELECT 'synthetic-discount-queue-'||uuid_generate_v4() FROM generate_series(1,34) RETURNING id)
        INSERT INTO organization_usage_discounts (organization_id,discount_basis_points,apply_since,saved_at,created_by,status,next_attempt_at)
        SELECT id,2500,clock_timestamp()-INTERVAL '2 days',clock_timestamp()-INTERVAL '1 day',$1,'applying',clock_timestamp()-INTERVAL '1 day' FROM org RETURNING organization_id", &[&user]).await.unwrap();
    let ids: Vec<Uuid> = rows.iter().map(|row| row.get(0)).collect();
    client
        .execute(
            "UPDATE organization_balance SET total_spent=1 WHERE organization_id=ANY($1)",
            &[&ids],
        )
        .await
        .unwrap();
    drop(client);
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    let mut seen = std::collections::HashSet::new();
    for _ in 0..34 {
        let claim = repo.claim_next().await.unwrap().unwrap();
        assert!(ids.contains(&claim.organization_id));
        assert!(
            seen.insert(claim.organization_id),
            "old failures must yield to every due organization"
        );
        assert!(repo.run_claimed(claim).await.is_err());
    }
    assert!(repo.claim_next().await.unwrap().is_none());
    // Keep this global-queue test's failed jobs isolated from later tests.
    f.db.pool()
        .get()
        .await
        .unwrap()
        .execute("DELETE FROM organizations WHERE id=ANY($1)", &[&ids])
        .await
        .unwrap();
}

#[tokio::test]
async fn deleting_discount_creator_preserves_rule_and_clears_audit_reference() {
    let f = fixture().await;
    let creator = Uuid::new_v4();
    let client = f.db.pool().get().await.unwrap();
    client.execute("INSERT INTO users (id,email,username,auth_provider,provider_user_id) VALUES ($1,$2,$3::VARCHAR,'test',$3::VARCHAR)", &[&creator,&format!("{creator}@example.test"),&creator.to_string()]).await.unwrap();
    drop(client);
    let repo = OrganizationUsageDiscountRepository::new(f.db.pool().clone());
    repo.save(f.org, BASIS_POINTS, None, creator).await.unwrap();
    let client = f.db.pool().get().await.unwrap();
    client
        .execute("DELETE FROM users WHERE id=$1", &[&creator])
        .await
        .unwrap();
    let row = client
        .query_one(
            "SELECT created_by,status FROM organization_usage_discounts WHERE organization_id=$1",
            &[&f.org],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Option<Uuid>>(0), None);
    assert_eq!(row.get::<_, String>(1), "active");
}
