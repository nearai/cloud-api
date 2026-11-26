//! Shared admission survives process replacement; usage settles once and expiry never bills an estimate.
use super::*;
use services::usage::budget::BudgetReservation;
use services::usage::{BillingIdentity, RecordUsageDbRequest, UsageRepository};

fn quote(org: &OrgFixture, model: &ModelFixture) -> BudgetReservation {
    BudgetReservation {
        credit_provider_policy_version: 0,
        serving_provider_type: None,
        reserve: true,
        id: Uuid::new_v4(),
        identity: BillingIdentity {
            organization_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model_id: model.id,
        },
        amount: 10,
        inference_type: InferenceType::ChatCompletion,
        estimator_version: 1,
        organization_max_unsettled: 200,
        key_max_unsettled: 200,
        reservation_ttl_seconds: 7200,
    }
}
fn receipt(q: &BudgetReservation, model: &ModelFixture) -> RecordUsageDbRequest {
    RecordUsageDbRequest {
        organization_id: q.identity.organization_id,
        workspace_id: q.identity.workspace_id,
        api_key_id: q.identity.api_key_id,
        model_id: q.identity.model_id,
        model_name: model.name.clone(),
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
    original.admit_budget(&q).await?;
    let response_id = Uuid::new_v4();
    let client = pool.get().await?;
    client.execute("INSERT INTO responses(id,workspace_id,api_key_id,model,status) VALUES($1,$2,$3,$4,'completed')", &[&response_id,&org.workspace_a_id,&org.api_key_a_id,&model.name]).await?;
    let mut req = receipt(&q, &model);
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
async fn admission_survives_writer_exit_then_expires_without_billing_and_accepts_late_usage(
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
    let writer = test_pool().await?;
    writer.current().unwrap().resize(1);
    let repo = OrganizationUsageRepository::new(writer.clone());
    let mut q = quote(&org, &model);
    q.reservation_ttl_seconds = 1234;
    repo.admit_budget(&q).await?;
    let pid: i32 = writer
        .get()
        .await?
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get(0);
    let client = pool.get().await?;
    client
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .await?;
    drop(repo);
    drop(writer);
    let restarted = OrganizationUsageRepository::new(pool.clone());
    let ttl: f64 = client.query_one("SELECT EXTRACT(EPOCH FROM expires_at-created_at)::FLOAT8 FROM usage_budget_reservations WHERE id=$1", &[&q.id]).await?.get(0);
    assert!((ttl - 1234.0).abs() < 1.0);
    restarted.expire_budgets().await?;
    assert_eq!(
        client
            .query_one(
                "SELECT state FROM usage_budget_reservations WHERE id=$1",
                &[&q.id]
            )
            .await?
            .get::<_, String>(0),
        "active"
    );
    client.execute("UPDATE usage_budget_reservations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", &[&q.id]).await?;
    restarted.expire_budgets().await?;
    let row = client.query_one("SELECT r.state,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, String>(0), "released");
    assert_eq!((row.get::<_, i64>(1), row.get::<_, i64>(2)), (0, 0));
    assert!(
        UsageRepository::record_usage(&restarted, receipt(&q, &model))
            .await?
            .was_inserted
    );
    assert!(
        !UsageRepository::record_usage(&restarted, receipt(&q, &model))
            .await?
            .was_inserted
    );
    let row = client.query_one("SELECT r.state,b.total_spent,reserved_credit(r.organization_id) FROM usage_budget_reservations r JOIN organization_balance b ON b.organization_id=r.organization_id WHERE r.id=$1", &[&q.id]).await?;
    assert_eq!(row.get::<_, String>(0), "settled");
    assert_eq!((row.get::<_, i64>(1), row.get::<_, i64>(2)), (30, 0));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

async fn fund(pool: &database::DbPool) -> anyhow::Result<OrgFixture> {
    let org = insert_org_fixture(pool).await?;
    set_limit(
        &OrganizationLimitsRepository::new(pool.clone()),
        org.org_id,
        "payment",
        100,
    )
    .await?;
    Ok(org)
}

// Compare durable accounting from a separate connection.
async fn account(pool: &database::DbPool, org: Uuid) -> anyhow::Result<(i64, i64, i64)> {
    let row = pool
        .get()
        .await?
        .query_one(
            "SELECT reserved_credit($1), total_spent,
         (SELECT count(*) FROM organization_usage_log WHERE organization_id=$1)
         FROM organization_balance WHERE organization_id=$1",
            &[&org],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2)))
}

#[tokio::test]
async fn release_reports_lock_contention_before_the_service_deadline() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "release-timeout").await?;
    let org = fund(&pool).await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let reservation = quote(&org, &model);
    repo.admit_budget(&reservation).await?;
    let mut client = pool.get().await?;
    let lock = client.transaction().await?;
    lock.query_one(
        "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
        &[&org.org_id],
    )
    .await?;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        repo.release_budget(org.org_id, reservation.id),
    )
    .await?
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(|e| e.code()),
        Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE)
    );
    assert_eq!(account(&pool, org.org_id).await?, (10, 0, 0));
    lock.rollback().await?;
    repo.release_budget(org.org_id, reservation.id).await?;
    assert_eq!(account(&pool, org.org_id).await?, (0, 0, 0));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn expiry_skips_a_locked_organization_and_revisits_it() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "expiry-progress").await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    // Fill an entire sweep page and leave one further organization eligible.
    let mut ids = Vec::new();
    for _ in 0..101 {
        let org = fund(&pool).await?;
        repo.admit_budget(&quote(&org, &model)).await?;
        ids.push(org.org_id);
    }
    ids.sort_unstable();
    let free = ids[100];
    let mut client = pool.get().await?;
    let lock = client.transaction().await?;
    lock.query(
        "SELECT id FROM organizations WHERE id=ANY($1) FOR UPDATE",
        &[&&ids[..99]],
    )
    .await?;
    pool.get().await?.execute(
        "UPDATE usage_budget_reservations SET expires_at=statement_timestamp() WHERE organization_id=ANY($1)",
        &[&ids],
    ).await?;
    // This organization can be selected, but its expiry write times out.
    lock.query_one(
        "SELECT id FROM usage_budget_reservations WHERE organization_id=$1 FOR UPDATE",
        &[&ids[99]],
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while account(&pool, free).await? != (0, 0, 0) {
            repo.expire_budgets().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(account(&pool, ids[0]).await?, (10, 0, 0));
    assert_eq!(account(&pool, ids[99]).await?, (10, 0, 0));
    lock.rollback().await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.query_one("SELECT count(*) FROM usage_budget_reservations WHERE organization_id=ANY($1) AND state='active'", &[&ids]).await?.get::<_, i64>(0) != 0 {
            repo.expire_budgets().await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    cleanup_usage_fixtures(&pool, &ids, &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_keys_share_credit_and_settling_a_retry_preserves_the_unknown_attempt(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "retry-ownership").await?;
    let org = fund(&pool).await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let mut first = quote(&org, &model);
    first.amount = 60;
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    second.identity.api_key_id = org.api_key_b_id;
    second.identity.workspace_id = org.workspace_b_id;
    let (a, b) = tokio::join!(repo.admit_budget(&first), repo.admit_budget(&second));
    use services::usage::UsageError::LimitExceeded;
    let held = match (a, b) {
        (Ok(_), Err(LimitExceeded(_))) => &first,
        (Err(LimitExceeded(_)), Ok(_)) => &second,
        other => panic!("one of the two keys must be admitted: {other:?}"),
    };
    assert_eq!(account(&pool, org.org_id).await?, (60, 0, 0));
    repo.mark_budget_unknown(org.org_id, held.id, 300).await?;
    let mut retry = held.clone();
    retry.id = Uuid::new_v4();
    retry.amount = 40;
    repo.admit_budget(&retry).await?;
    UsageRepository::record_usage(&repo, receipt(&retry, &model)).await?;
    assert_eq!(account(&pool, org.org_id).await?, (60, 30, 1));
    let client = pool.get().await?;
    for (id, state, unknown) in [(held.id, "active", true), (retry.id, "settled", false)] {
        let row = client.query_one("SELECT state,outcome_unknown_at IS NOT NULL FROM usage_budget_reservations WHERE id=$1", &[&id]).await?;
        assert_eq!(
            (row.get::<_, String>(0).as_str(), row.get::<_, bool>(1)),
            (state, unknown)
        );
    }
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn duplicate_receipts_racing_expiry_and_unknown_charge_once() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "settlement-race").await?;
    let org = fund(&pool).await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let q = quote(&org, &model);
    repo.admit_budget(&q).await?;
    pool.get()
        .await?
        .execute(
            "UPDATE usage_budget_reservations SET expires_at=now() WHERE id=$1",
            &[&q.id],
        )
        .await?;
    let (a, b, unknown, sweep) = tokio::join!(
        UsageRepository::record_usage(&repo, receipt(&q, &model)),
        UsageRepository::record_usage(&repo, receipt(&q, &model)),
        repo.mark_budget_unknown(org.org_id, q.id, 30),
        repo.expire_budgets()
    );
    let (a, b) = (a?, b?);
    unknown?;
    sweep?;
    assert_eq!(a.id, b.id);
    assert_ne!(a.was_inserted, b.was_inserted);
    assert_eq!(account(&pool, org.org_id).await?, (0, 30, 1));
    assert_eq!(a.funded_amount, Some(30));
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn disconnected_writer_rolls_back_before_commit_and_replays_after_commit(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "settlement-commit").await?;
    let org = fund(&pool).await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    for committed in [false, true] {
        let q = quote(&org, &model);
        repo.admit_budget(&q).await?;
        let mut writer = pool.get().await?;
        let tx = writer.transaction().await?;
        let pid: i32 = tx.query_one("SELECT pg_backend_pid()", &[]).await?.get(0);
        tx.query_one(
            "SELECT was_inserted FROM record_organization_usage(gen_random_uuid(), $1,$2,$3,$4,$5,
             20,10,0,0,20::bigint,10::bigint,30::bigint,'chat_completion',NULL,NULL,NULL,NULL,NULL,
             NULL,NULL,NULL,NULL,false,jsonb_build_object('budget_reservation_id',$6::uuid),NULL,NULL,
             ARRAY['payment'],'test',true)",
            &[&org.org_id,&q.identity.workspace_id,&q.identity.api_key_id,&model.id,&model.name,&q.id]).await?;
        let observer = pool.get().await?;
        if committed {
            // Do not deliver the commit reply to the client before its connection is lost.
            let commit = tx.batch_execute("COMMIT; SELECT pg_sleep(30)");
            let disconnect = async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let state: String = observer
                            .query_one(
                                "SELECT state FROM usage_budget_reservations WHERE id=$1",
                                &[&q.id],
                            )
                            .await?
                            .get(0);
                        if state == "settled" {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
                observer
                    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                    .await?;
                Ok::<_, anyhow::Error>(())
            };
            let (ack, killed) = tokio::join!(commit, disconnect);
            killed?;
            assert!(ack.is_err());
            assert_eq!(account(&pool, org.org_id).await?, (0, 60, 2));
        } else {
            assert_eq!(account(&pool, org.org_id).await?, (10, 0, 0));
            observer
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await?;
            assert!(tx.query_one("SELECT 1", &[]).await.is_err());
            assert_eq!(account(&pool, org.org_id).await?, (10, 0, 0));
        }
        drop(tx);
        drop(writer);
        let replay = UsageRepository::record_usage(&repo, receipt(&q, &model)).await?;
        assert_eq!(replay.was_inserted, !committed);
        assert_eq!(
            account(&pool, org.org_id).await?,
            if committed { (0, 60, 2) } else { (0, 30, 1) }
        );
    }
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn admission_selects_the_available_provider_policy_contract() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "provider-admission").await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    set_limit(
        &OrganizationLimitsRepository::new(pool.clone()),
        org.org_id,
        "payment",
        100,
    )
    .await?;
    let mut reservation = quote(&org, &model);
    reservation.credit_provider_policy_version =
        repository.credit_provider_policy_version().await?;
    reservation.serving_provider_type = Some(services::usage::ServedProviderType::Chutes);

    repository.admit_budget(&reservation).await?;
    let row = pool
        .get()
        .await?
        .query_one(
            "SELECT credit_type, amount FROM usage_budget_funding WHERE reservation_id=$1",
            &[&reservation.id],
        )
        .await?;

    assert_eq!(row.get::<_, String>(0), "payment");
    assert_eq!(row.get::<_, i64>(1), reservation.amount);
    repository
        .release_budget(org.org_id, reservation.id)
        .await?;
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}
