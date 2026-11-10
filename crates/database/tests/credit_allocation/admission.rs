//! Shared admission survives process replacement; usage settles once and expiry never bills an estimate.
use super::*;
use services::usage::budget::{BudgetReservation, UsageFeatures};
use services::usage::{BillingIdentity, ModelPricing, RecordUsageDbRequest, UsageRepository};

fn quote(org: &OrgFixture, model: &ModelFixture) -> BudgetReservation {
    BudgetReservation {
        id: Uuid::new_v4(),
        identity: BillingIdentity {
            organization_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model_id: model.id,
        },
        amount: 10,
        pricing: ModelPricing {
            id: model.id,
            model_name: model.name.clone(),
            input_cost_per_token: 1,
            output_cost_per_token: 1,
            cost_per_image: 0,
            cache_read_cost_per_token: None,
            text_pricing: None,
        },
        estimate: UsageFeatures::from_json(
            InferenceType::ChatCompletion,
            &serde_json::json!({"max_tokens":1}),
        )
        .unwrap()
        .estimate(&Default::default(), &model.name)
        .unwrap(),
        organization_max_unsettled: 200,
        key_max_unsettled: 200,
        reservation_ttl_seconds: 7200,
    }
}
fn receipt(q: &BudgetReservation) -> RecordUsageDbRequest {
    RecordUsageDbRequest {
        organization_id: q.identity.organization_id,
        workspace_id: q.identity.workspace_id,
        api_key_id: q.identity.api_key_id,
        model_id: q.identity.model_id,
        model_name: q.pricing.model_name.clone(),
        input_tokens: 20,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        billing_details: Some(serde_json::json!({"budget_reservation_id":q.id})),
        service_tier: None,
        context_band: None,
        input_cost: 20,
        output_cost: 10,
        total_cost: 30,
        inference_type: InferenceType::ChatCompletion,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: None,
        provider_request_id: None,
        stop_reason: None,
        response_id: None,
        image_count: None,
        provider_attribution: Default::default(),
        dedupe_on_conflict: true,
    }
}

#[tokio::test]
async fn settlement_survives_response_erasure_and_lost_ack() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "admitted-settlement").await?;
    set_limit(
        &OrganizationLimitsRepository::new(pool.clone()),
        org.org_id,
        "payment",
        100,
    )
    .await?;
    let q = quote(&org, &model);
    let original = OrganizationUsageRepository::new(pool.clone());
    original.reserve_and_dispatch(&q).await?;
    let response_id = Uuid::new_v4();
    let client = pool.get().await?;
    client.execute("INSERT INTO responses(id,workspace_id,api_key_id,model,status) VALUES($1,$2,$3,$4,'completed')", &[&response_id,&org.workspace_a_id,&org.api_key_a_id,&model.name]).await?;
    let mut req = receipt(&q);
    req.response_id = Some(response_id.into());
    drop(original);
    client
        .execute("DELETE FROM responses WHERE id=$1", &[&response_id])
        .await?;
    let restarted = OrganizationUsageRepository::new(pool.clone());
    assert!(
        UsageRepository::record_usage(&restarted, req.clone())
            .await?
            .was_inserted
    );
    // The existing report transport can repeat a delivery after a lost acknowledgement.
    let replay = UsageRepository::record_usage(&restarted, req).await?;
    assert!(!replay.was_inserted);
    assert_eq!(replay.total_cost, 30);
    assert!(replay.response_id.is_none());
    assert_eq!(replay.inference_id, Some(q.id));
    let row = client.query_one("SELECT r.amount,r.state,r.actual_cost,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, i64>(0), 10, "the estimate is immutable");
    assert_eq!(row.get::<_, String>(1), "settled");
    assert_eq!(row.get::<_, i64>(2), 30);
    assert_eq!((row.get::<_, i64>(3), row.get::<_, i64>(4)), (30, 0));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn expiry_releases_unknown_work_without_billing_and_late_reports_still_settle(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "budget-expiry").await?;
    set_limit(
        &OrganizationLimitsRepository::new(pool.clone()),
        org.org_id,
        "payment",
        100,
    )
    .await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let mut q = quote(&org, &model);
    q.reservation_ttl_seconds = 1234;
    repo.reserve_and_dispatch(&q).await?;
    let client = pool.get().await?;
    let ttl: f64 = client.query_one("SELECT EXTRACT(EPOCH FROM expires_at-created_at)::FLOAT8 FROM usage_budget_reservations WHERE id=$1", &[&q.id]).await?.get(0);
    assert!((ttl - 1234.0).abs() < 1.0);
    repo.expire_budgets().await?;
    assert_eq!(
        client
            .query_one(
                "SELECT state FROM usage_budget_reservations WHERE id=$1",
                &[&q.id]
            )
            .await?
            .get::<_, String>(0),
        "dispatched"
    );
    client.execute("UPDATE usage_budget_reservations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", &[&q.id]).await?;
    drop(repo);
    let restarted = OrganizationUsageRepository::new(pool.clone());
    restarted.expire_budgets().await?;
    let row = client.query_one("SELECT r.state,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, String>(0), "released");
    assert_eq!((row.get::<_, i64>(1), row.get::<_, i64>(2)), (0, 0));
    assert!(
        UsageRepository::record_usage(&restarted, receipt(&q))
            .await?
            .was_inserted
    );
    assert!(
        !UsageRepository::record_usage(&restarted, receipt(&q))
            .await?
            .was_inserted
    );
    let row = client.query_one("SELECT r.state,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, String>(0), "settled");
    assert_eq!((row.get::<_, i64>(1), row.get::<_, i64>(2)), (30, 0));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}
