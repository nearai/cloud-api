#[allow(dead_code)]
mod support;

use database::models::{RecordUsageRequest, UpdateOrganizationLimitsDbRequest};
use database::repositories::credit_transfers::{
    CreditTransferRepository, TransferActor, TransferError, TransferRequest,
};
use database::repositories::{OrganizationLimitsRepository, OrganizationUsageRepository};
use database::DbPool;
use serde_json::{json, Value};
use support::{insert_model, insert_org_fixture, test_pool, ModelFixture, OrgFixture};
use uuid::Uuid;

async fn organization(pool: &DbPool) -> anyhow::Result<(OrgFixture, Uuid)> {
    let org = insert_org_fixture(pool).await?;
    let client = pool.get().await?;
    let owner: Uuid = client
        .query_one(
            "SELECT created_by_user_id FROM workspaces WHERE id = $1",
            &[&org.workspace_a_id],
        )
        .await?
        .get(0);
    client
        .execute(
            r#"
                INSERT INTO organization_members (organization_id, user_id, role)
                VALUES ($1, $2, 'owner')
                "#,
            &[&org.org_id, &owner],
        )
        .await?;
    client
        .execute(
            r#"
                INSERT INTO organization_balance (organization_id)
                VALUES ($1)
                ON CONFLICT DO NOTHING
                "#,
            &[&org.org_id],
        )
        .await?;
    Ok((org, owner))
}

async fn fund(pool: &DbPool, org: Uuid, kind: &str, amount: i64) -> anyhow::Result<()> {
    OrganizationLimitsRepository::new(pool.clone())
        .update_limits(
            org,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: amount,
                credit_type: kind.into(),
                source: Some(
                    if kind == "staking_farm" {
                        "house-of-stake"
                    } else {
                        "test-payment"
                    }
                    .into(),
                ),
                currency: "USD".into(),
                changed_by: Some("test".into()),
                change_reason: Some("test posted credits".into()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    Ok(())
}

fn request(recipient: Uuid, source: &str, amount: i64) -> TransferRequest {
    TransferRequest {
        recipient_organization_id: recipient,
        source_credit_type: source.into(),
        amount,
        memo: "  settlement  ".into(),
        metadata: serde_json::from_value(json!({"reference": "opaque"})).unwrap(),
    }
}

fn code<T: std::fmt::Debug>(result: anyhow::Result<T>) -> &'static str {
    result.unwrap_err().downcast::<TransferError>().unwrap().0
}

fn usage(org: &OrgFixture, model: &ModelFixture, cost: i64) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: org.org_id,
        workspace_id: org.workspace_a_id,
        api_key_id: org.api_key_a_id,
        model_id: model.id,
        model_name: model.name.clone(),
        input_tokens: 1,
        output_tokens: 0,
        input_cost: cost,
        output_cost: 0,
        total_cost: cost,
        inference_type: "chat_completion".into(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: None,
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

#[tokio::test]
async fn transfers_conserve_capacity_replay_and_settle_without_exposing_recipient_debt(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, recipient_owner) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    let recipient_actor = TransferActor::Owner(recipient_owner);
    let model = insert_model(&pool, "transfer-accounting").await?;
    let usage_repository = OrganizationUsageRepository::new(pool.clone());
    fund(&pool, sender.org_id, "staking_farm", 100).await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed reseller")))
        .await?;
    usage_repository
        .record_usage(usage(&sender, &model, 20))
        .await?;
    usage_repository
        .record_usage(usage(&recipient, &model, 10))
        .await?;
    let terms = request(recipient.org_id, "staking_farm", 30);

    let (result, replay) = repository
        .transfer(sender.org_id, &actor, "order-1", &terms)
        .await?;

    assert!(!replay);
    assert_eq!(result["sender_snapshot"][0]["gross_limit"], 100);
    assert_eq!(result["sender_snapshot"][0]["consumed"], 20);
    assert_eq!(result["sender_snapshot"][0]["transferred_out"], 30);
    assert_eq!(result["sender_snapshot"][0]["available"], 50);
    assert_eq!(result["staking_evidence"]["own_earned_total"], 100);
    assert!(result.get("recipient_snapshot").is_none());
    let history = repository
        .history(recipient.org_id, &recipient_actor, None, None, None, 10)
        .await?;
    assert!(history["transfers"][0].get("sender_snapshot").is_none());
    assert_eq!(
        history["transfers"][0]["recipient_snapshot"]["amount_applied_to_debt"],
        10
    );
    assert_eq!(
        history["transfers"][0]["recipient_snapshot"]["credits"][0]["available"],
        20
    );

    fund(&pool, sender.org_id, "staking_farm", 105).await?;
    repository
        .policy(sender.org_id, Some((false, owner, "Emergency stop")))
        .await?;
    let (retry, replay) = repository
        .transfer(sender.org_id, &actor, "order-1", &terms)
        .await?;

    assert!(replay);
    assert_eq!(retry, result);
    assert_eq!(
        repository.capacity(sender.org_id, &actor).await?["credits"][0]["available"],
        55
    );
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "order-2", &terms)
                .await
        ),
        "sender_not_enabled"
    );
    let mut changed = terms.clone();
    changed.memo.push('!');
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "order-1", &changed)
                .await
        ),
        "idempotency_conflict"
    );

    repository
        .policy(
            recipient.org_id,
            Some((true, recipient_owner, "Reviewed reseller")),
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(
                    recipient.org_id,
                    &recipient_actor,
                    "hop-2",
                    &request(sender.org_id, "payment", 1)
                )
                .await
        ),
        "insufficient_posted_capacity"
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    recipient.org_id,
                    &recipient_actor,
                    "hop-2",
                    &request(sender.org_id, "transfer", 1)
                )
                .await
        ),
        "invalid_transfer_terms"
    );
    fund(&pool, recipient.org_id, "payment", 5).await?;
    repository
        .transfer(
            recipient.org_id,
            &recipient_actor,
            "own-payment",
            &request(sender.org_id, "payment", 5),
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(
                    recipient.org_id,
                    &recipient_actor,
                    "mixed",
                    &request(sender.org_id, "payment", 1)
                )
                .await
        ),
        "insufficient_posted_capacity"
    );

    // Inference posting uses the replaced SQL function, not the Rust allocator.
    usage_repository
        .record_usage(usage(&recipient, &model, 25))
        .await?;
    let client = pool.get().await?;
    let allocated: i64 = client
        .query_one(
            r#"
                SELECT amount
                FROM organization_credit_consumption
                WHERE organization_id = $1
                AND credit_type = 'transfer'
                "#,
            &[&recipient.org_id],
        )
        .await?
        .get(0);
    let debt: i64 = client
        .query_one(
            r#"
                SELECT unresolved_unfunded_amount
                FROM organization_balance
                WHERE organization_id = $1
                "#,
            &[&recipient.org_id],
        )
        .await?
        .get(0);

    assert_eq!(allocated, 30);
    assert_eq!(debt, 5);
    assert_eq!(
        code(
            repository
                .transfer(
                    recipient.org_id,
                    &recipient_actor,
                    "debt",
                    &request(sender.org_id, "payment", 1)
                )
                .await
        ),
        "sender_not_reconciled"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_retries_distinct_keys_and_credentials_serialize() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    fund(&pool, sender.org_id, "payment", 100).await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed reseller")))
        .await?;
    let terms = request(recipient.org_id, "payment", 60);

    let (first, second) = tokio::join!(
        repository.transfer(sender.org_id, &actor, "same", &terms),
        repository.transfer(sender.org_id, &actor, "same", &terms),
    );
    let first = first?;
    let second = second?;

    assert_eq!(first.0, second.0);
    assert_ne!(first.1, second.1);
    let remaining = request(recipient.org_id, "payment", 40);
    let (first, second) = tokio::join!(
        repository.transfer(sender.org_id, &actor, "different-a", &remaining),
        repository.transfer(sender.org_id, &actor, "different-b", &remaining),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(
        code(if first.is_err() { first } else { second }),
        "insufficient_posted_capacity"
    );

    let credential = repository
        .credential(
            sender.org_id,
            owner,
            None,
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        )
        .await?;
    let credential_actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            credential["token"].as_str().unwrap(),
        ));
    let retry = repository
        .transfer(sender.org_id, &credential_actor, "same", &terms)
        .await?;
    assert!(retry.1);
    assert_eq!(
        retry.0,
        first_result(&repository, sender.org_id, &actor, "same").await?
    );
    repository
        .credential(
            sender.org_id,
            owner,
            Some(serde_json::from_value(credential["id"].clone())?),
            None,
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &credential_actor, "same", &terms)
                .await
        ),
        "credential_refused"
    );

    let client = pool.get().await?;
    let total: i64 = client
        .query_one(
            r#"
                SELECT SUM(amount)::BIGINT
                FROM organization_credit_transfers
                WHERE sender_organization_id = $1
                "#,
            &[&sender.org_id],
        )
        .await?
        .get(0);
    assert_eq!(total, 100);
    assert!(client
        .execute(
            "DELETE FROM organization_credit_transfers WHERE sender_organization_id = $1",
            &[&sender.org_id]
        )
        .await
        .is_err());
    Ok(())
}

async fn first_result(
    repository: &CreditTransferRepository,
    org: Uuid,
    actor: &TransferActor,
    key: &str,
) -> anyhow::Result<Value> {
    let mut result = repository
        .history(org, actor, None, Some(key), None, 1)
        .await?["transfers"][0]
        .clone();
    result.as_object_mut().unwrap().remove("idempotency_key");
    Ok(result)
}

#[tokio::test]
async fn precommit_failures_do_not_reserve_keys_or_leave_partial_accounting() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, recipient_owner) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    fund(&pool, sender.org_id, "payment", 100).await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed")))
        .await?;
    let terms = request(recipient.org_id, "payment", 30);
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&recipient.org_id],
        )
        .await?;

    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "reusable", &terms)
                .await
        ),
        "organization_unavailable"
    );
    client
        .execute(
            "UPDATE organizations SET is_active = true WHERE id = $1",
            &[&recipient.org_id],
        )
        .await?;
    client
        .execute(
            r#"
                UPDATE organization_balance
                SET legacy_unattributed_amount = 1
                WHERE organization_id = $1
                "#,
            &[&sender.org_id],
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "reusable", &terms)
                .await
        ),
        "sender_not_reconciled"
    );
    client
        .execute(
            r#"
                UPDATE organization_balance
                SET legacy_unattributed_amount = 0
                WHERE organization_id = $1
                "#,
            &[&sender.org_id],
        )
        .await?;

    // Fail at ledger insertion after both projections, recipient issuance and
    // settlement have run. The constraint only targets this test's sender.
    let constraint = format!("transfer_fault_{}", sender.org_id.simple());
    client
        .batch_execute(&format!(
            "ALTER TABLE organization_credit_transfers ADD CONSTRAINT {constraint} \
         CHECK (sender_organization_id <> '{}') NOT VALID",
            sender.org_id,
        ))
        .await?;

    let failed = repository
        .transfer(sender.org_id, &actor, "reusable", &terms)
        .await;

    client
        .batch_execute(&format!(
            "ALTER TABLE organization_credit_transfers DROP CONSTRAINT {constraint}",
        ))
        .await?;
    assert!(failed.is_err());
    assert_eq!(
        repository.capacity(sender.org_id, &actor).await?["credits"][0]["available"],
        100
    );
    assert!(repository
        .capacity(recipient.org_id, &TransferActor::Owner(recipient_owner))
        .await?["credits"]
        .as_array()
        .unwrap()
        .is_empty());

    // A real completed transfer fills the recipient's cumulative BIGINT limit.
    // A subsequent overflow must leave the sender and its key untouched.
    let (large_sender, large_owner) = organization(&pool).await?;
    fund(&pool, large_sender.org_id, "payment", i64::MAX).await?;
    repository
        .policy(large_sender.org_id, Some((true, large_owner, "Reviewed")))
        .await?;
    repository
        .transfer(
            large_sender.org_id,
            &TransferActor::Owner(large_owner),
            "max",
            &request(recipient.org_id, "payment", i64::MAX),
        )
        .await?;

    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "reusable", &terms)
                .await
        ),
        "capacity_overflow"
    );
    let capacity = repository.capacity(sender.org_id, &actor).await?;
    assert_eq!(capacity["credits"][0]["transferred_out"], 0);
    assert_eq!(capacity["credits"][0]["available"], 100);
    assert!(repository
        .history(sender.org_id, &actor, None, Some("reusable"), None, 1)
        .await?["transfers"]
        .as_array()
        .unwrap()
        .is_empty());

    let (other, _) = organization(&pool).await?;
    repository
        .transfer(
            sender.org_id,
            &actor,
            "reusable",
            &request(other.org_id, "payment", 30),
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &TransferActor::Owner(recipient_owner),
                    "reusable",
                    &terms
                )
                .await
        ),
        "credential_refused"
    );
    assert!(fund(&pool, other.org_id, "transfer", 500).await.is_err());
    Ok(())
}

#[tokio::test]
async fn rust_service_allocation_and_inflight_overage_use_effective_capacity() -> anyhow::Result<()>
{
    use database::repositories::{OrganizationServiceUsageRepository, RecordServiceUsageRequest};

    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    fund(&pool, sender.org_id, "payment", 100).await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed")))
        .await?;
    repository
        .transfer(
            sender.org_id,
            &actor,
            "distribution",
            &request(recipient.org_id, "payment", 80),
        )
        .await?;
    let service_id = Uuid::new_v4();
    pool.get()
        .await?
        .execute(
            r#"
                INSERT INTO services (id, service_name, display_name, unit, cost_per_unit)
                VALUES ($1, $2, 'Transfer test', 'request', 30)
                "#,
            &[&service_id, &format!("transfer-service-{service_id}")],
        )
        .await?;
    let usage = RecordServiceUsageRequest {
        organization_id: sender.org_id,
        workspace_id: sender.workspace_a_id,
        api_key_id: sender.api_key_a_id,
        service_id,
        quantity: 1,
        total_cost: 30,
        inference_id: Some(Uuid::new_v4()),
    };

    // This models a request admitted before distribution, completing after it.
    // Admission does not reserve funds: the posted overage blocks further sends.
    let recorded = OrganizationServiceUsageRepository::new(pool.clone())
        .record_usage(&usage)
        .await?;

    assert_eq!(recorded.funded_amount, Some(20));
    assert_eq!(recorded.unfunded_amount, Some(10));
    assert_eq!(
        recorded.credit_allocations.unwrap()[0].credit_type,
        "payment"
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "blocked",
                    &request(recipient.org_id, "payment", 1)
                )
                .await
        ),
        "sender_not_reconciled"
    );
    fund(&pool, sender.org_id, "payment", 115).await?;
    let capacity = repository.capacity(sender.org_id, &actor).await?;

    assert_eq!(capacity["credits"][0]["gross_limit"], 115);
    assert_eq!(capacity["credits"][0]["transferred_out"], 80);
    assert_eq!(capacity["credits"][0]["consumed"], 30);
    assert_eq!(capacity["credits"][0]["available"], 5);
    Ok(())
}

#[tokio::test]
async fn staking_sync_and_source_lifecycle_never_reset_distribution() -> anyhow::Result<()> {
    use database::repositories::OrganizationStakingFarmSourcesRepository;
    use services::staking_farm::StakingFarmRepository;

    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let staking = OrganizationStakingFarmSourcesRepository::new(pool.clone());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    let client = pool.get().await?;
    client
        .execute(
            r#"INSERT INTO organization_staking_farm_sources (
            organization_id, near_account_id, network_id, contract_id,
            farm_product_id, credit_nano_usd_per_reward_unit, status
        ) VALUES ($1, $2, 'testnet', 'stake.test', 'cloud-credits', 1, 'active')"#,
            &[
                &sender.org_id,
                &format!("transfer-{}.test", sender.org_id.simple()),
            ],
        )
        .await?;
    staking
        .update_staking_farm_limit(sender.org_id, 100, None)
        .await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed")))
        .await?;
    repository
        .transfer(
            sender.org_id,
            &actor,
            "staking",
            &request(recipient.org_id, "staking_farm", 30),
        )
        .await?;

    for issuance in [105, 105, 90] {
        staking
            .update_staking_farm_limit(sender.org_id, issuance, None)
            .await?;
        let position = repository.capacity(sender.org_id, &actor).await?;

        assert_eq!(position["own_staking_earned_total"], 105);
        assert_eq!(position["credits"][0]["available"], 75);
        assert_eq!(position["credits"][0]["transferred_out"], 30);
    }

    client
        .execute(
            r#"
                UPDATE organization_staking_farm_sources
                SET status = 'disconnected'
                WHERE organization_id = $1
                "#,
            &[&sender.org_id],
        )
        .await?;
    assert!(
        !staking
            .update_staking_farm_limit(sender.org_id, 120, None)
            .await?
    );
    client
        .execute(
            r#"
                UPDATE organization_staking_farm_sources
                SET status = 'active'
                WHERE organization_id = $1
                "#,
            &[&sender.org_id],
        )
        .await?;
    staking
        .update_staking_farm_limit(sender.org_id, 110, None)
        .await?;

    let position = repository.capacity(sender.org_id, &actor).await?;
    assert_eq!(position["own_staking_earned_total"], 110);
    assert_eq!(position["credits"][0]["available"], 80);
    assert_eq!(position["credits"][0]["transferred_out"], 30);
    Ok(())
}

#[tokio::test]
async fn opposite_transfers_and_concurrent_usage_keep_conservation() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (a, a_owner) = organization(&pool).await?;
    let (b, b_owner) = organization(&pool).await?;
    let a_actor = TransferActor::Owner(a_owner);
    let b_actor = TransferActor::Owner(b_owner);
    for (org, owner) in [(a.org_id, a_owner), (b.org_id, b_owner)] {
        fund(&pool, org, "payment", 100).await?;
        repository
            .policy(org, Some((true, owner, "Reviewed")))
            .await?;
    }
    let to_b = request(b.org_id, "payment", 50);
    let to_a = request(a.org_id, "payment", 50);

    let (a_result, b_result) = tokio::join!(
        repository.transfer(a.org_id, &a_actor, "opposite-a", &to_b),
        repository.transfer(b.org_id, &b_actor, "opposite-b", &to_a),
    );
    a_result?;
    b_result?;

    let model = insert_model(&pool, "concurrent-transfer-usage").await?;
    let usage_repository = OrganizationUsageRepository::new(pool.clone());
    let remaining = request(b.org_id, "payment", 50);
    let (transfer, charge) = tokio::join!(
        repository.transfer(a.org_id, &a_actor, "remaining", &remaining),
        usage_repository.record_usage(usage(&a, &model, 80)),
    );
    charge?;
    if transfer.is_err() {
        assert_eq!(code(transfer), "insufficient_posted_capacity");
    }

    let client = pool.get().await?;
    let row = client
        .query_one(
            r#"SELECT t.outgoing, COALESCE(c.amount, 0)::BIGINT AS consumed
           FROM organization_credit_transfer_totals t
           LEFT JOIN organization_credit_consumption c USING (organization_id, credit_type)
           WHERE t.organization_id = $1 AND t.credit_type = 'payment'"#,
            &[&a.org_id],
        )
        .await?;
    assert!(row.get::<_, i64>("outgoing") + row.get::<_, i64>("consumed") <= 100);
    Ok(())
}

#[tokio::test]
async fn trace_commons_credit_cannot_fund_a_transfer() -> anyhow::Result<()> {
    use database::repositories::trace_commons_credit::TraceCommonsCreditRepository;

    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    // A whitelisted, reconciled reseller whose only credit is Trace Commons.
    TraceCommonsCreditRepository::new(pool.clone())
        .set_cumulative(sender.org_id, 100, "sha256:earned", None)
        .await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed reseller")))
        .await?;

    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "trace-commons",
                    &request(recipient.org_id, "trace_commons", 10)
                )
                .await
        ),
        "invalid_transfer_terms"
    );
    // Neither transferable source has capacity, so trace_commons cannot leak
    // through them either.
    for source in ["payment", "staking_farm"] {
        assert_eq!(
            code(
                repository
                    .transfer(
                        sender.org_id,
                        &actor,
                        source,
                        &request(recipient.org_id, source, 10)
                    )
                    .await
            ),
            "insufficient_posted_capacity"
        );
    }

    let client = pool.get().await?;
    let projections: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM organization_credit_transfer_totals WHERE organization_id = ANY($1)",
            &[&vec![sender.org_id, recipient.org_id]],
        )
        .await?
        .get(0);
    assert_eq!(projections, 0);
    // The database refuses a trace_commons projection or ledger row as well.
    assert!(client
        .execute(
            "INSERT INTO organization_credit_transfer_totals (organization_id, credit_type, outgoing) VALUES ($1, 'trace_commons', 1)",
            &[&sender.org_id],
        )
        .await
        .is_err());
    let capacity = repository.capacity(sender.org_id, &actor).await?;
    assert_eq!(capacity["credits"][0]["credit_type"], "trace_commons");
    assert_eq!(capacity["credits"][0]["transferred_out"], 0);
    assert_eq!(capacity["credits"][0]["available"], 100);
    Ok(())
}

#[tokio::test]
async fn trace_commons_writes_serialize_with_transfers_and_leave_projections_alone(
) -> anyhow::Result<()> {
    use database::repositories::trace_commons_credit::TraceCommonsCreditRepository;

    let pool = test_pool().await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let trace_commons = TraceCommonsCreditRepository::new(pool.clone());
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let actor = TransferActor::Owner(owner);
    fund(&pool, sender.org_id, "payment", 100).await?;
    repository
        .policy(sender.org_id, Some((true, owner, "Reviewed reseller")))
        .await?;

    // Hold the shared organization accounting lock: a trace_commons write must
    // wait for it (and time out), exactly as a transfer does.
    let mut holder = pool.get().await?;
    let held = holder.transaction().await?;
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&sender.org_id],
    )
    .await?;
    let blocked = trace_commons
        .set_cumulative(sender.org_id, 50, "sha256:held", None)
        .await
        .unwrap_err();
    assert_eq!(
        blocked
            .downcast_ref::<database::repositories::trace_commons_credit::TraceCommonsCreditError>(
            ),
        Some(&database::repositories::trace_commons_credit::TraceCommonsCreditError::Conflict)
    );
    held.rollback().await?;
    drop(holder);

    let mut tasks = Vec::new();
    for step in 0..5_i64 {
        let repository = repository.clone();
        let trace_commons = trace_commons.clone();
        let actor = actor.clone();
        let (sender, recipient) = (sender.org_id, recipient.org_id);
        tasks.push(tokio::spawn(async move {
            trace_commons
                .set_cumulative(sender, 10 * (step + 1), "sha256:concurrent", None)
                .await?;
            repository
                .transfer(
                    sender,
                    &actor,
                    &format!("concurrent-{step}"),
                    &request(recipient, "payment", 10),
                )
                .await?;
            anyhow::Ok(())
        }));
    }
    for task in tasks {
        task.await??;
    }

    let capacity = repository.capacity(sender.org_id, &actor).await?;
    let credits = capacity["credits"].as_array().unwrap();
    let position = |kind: &str| {
        credits
            .iter()
            .find(|credit| credit["credit_type"] == kind)
            .cloned()
            .unwrap()
    };
    assert_eq!(position("payment")["transferred_out"], 50);
    assert_eq!(position("payment")["available"], 50);
    assert_eq!(position("trace_commons")["gross_limit"], 50);
    assert_eq!(position("trace_commons")["transferred_out"], 0);
    assert_eq!(position("trace_commons")["effective_capacity"], 50);
    let types: Vec<String> = pool
        .get()
        .await?
        .query(
            "SELECT credit_type FROM organization_credit_transfer_totals WHERE organization_id = ANY($1) ORDER BY credit_type",
            &[&vec![sender.org_id, recipient.org_id]],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(types, ["payment", "transfer"]);
    Ok(())
}
