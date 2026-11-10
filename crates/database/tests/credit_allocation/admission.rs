//! Real persistence boundaries: receipt committed, executor lost, and ledger ACK lost.
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
async fn persisted_receipt_survives_executor_restart_response_erasure_and_lost_ack(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "durable-receipt").await?;
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
    original.persist_receipt(q.id, &req).await?;
    drop(original); // No ledger post happened in this process.
    client
        .execute("DELETE FROM responses WHERE id=$1", &[&response_id])
        .await?;
    let restarted = OrganizationUsageRepository::new(pool.clone());
    restarted.recover_receipts().await?;
    // Re-send after a lost acknowledgement: same admission and no provider id.
    restarted.persist_receipt(q.id, &req).await?;
    let replay = UsageRepository::record_usage(&restarted, req).await?;
    assert!(!replay.was_inserted);
    assert_eq!(replay.total_cost, 30);
    assert!(replay.response_id.is_none());
    assert_eq!(replay.inference_id, Some(q.id));
    let row = client.query_one("SELECT r.amount,r.state,r.actual_cost,r.receipt,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, i64>(0), 10, "the estimate is immutable");
    assert_eq!(row.get::<_, String>(1), "settled");
    assert_eq!(row.get::<_, i64>(2), 30);
    assert!(row.get::<_, Option<serde_json::Value>>(3).is_none());
    assert_eq!((row.get::<_, i64>(4), row.get::<_, i64>(5)), (30, 0));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn failed_old_receipts_do_not_starve_later_receipts_and_unknown_executions_stay_held(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "receipt-recovery-progress").await?;
    set_limit(
        &OrganizationLimitsRepository::new(pool.clone()),
        org.org_id,
        "payment",
        10000,
    )
    .await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let client = pool.get().await?;
    for _ in 0..101 {
        let q = quote(&org, &model);
        repo.reserve_and_dispatch(&q).await?;
        // A future/invalid payload remains held for investigation, but must not monopolize recovery.
        client
            .execute(
                "UPDATE usage_budget_reservations SET receipt='{}'::jsonb WHERE id=$1",
                &[&q.id],
            )
            .await?;
    }
    let unknown = quote(&org, &model);
    repo.reserve_and_dispatch(&unknown).await?;
    let q = quote(&org, &model);
    repo.reserve_and_dispatch(&q).await?;
    repo.persist_receipt(q.id, &receipt(&q)).await?;
    for _ in 0..3 {
        repo.recover_receipts().await?;
    }
    let row = client
        .query_one(
            "SELECT state, receipt FROM usage_budget_reservations WHERE id=$1",
            &[&q.id],
        )
        .await?;
    assert_eq!(row.get::<_, String>(0), "settled");
    assert!(row.get::<_, Option<serde_json::Value>>(1).is_none());
    let held: i64 = client
        .query_one("SELECT reserved_credit($1)", &[&org.org_id])
        .await?
        .get(0);
    assert_eq!(held, 1020);
    assert_eq!(
        client
            .query_one(
                "SELECT state FROM usage_budget_reservations WHERE id=$1",
                &[&unknown.id]
            )
            .await?
            .get::<_, String>(0),
        "dispatched"
    );
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}
