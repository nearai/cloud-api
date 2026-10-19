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
        amount: Some(amount),
        recipient_cumulative: None,
        payout_program: None,
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

    let (first, second) = contended_transfers(
        &pool,
        sender.org_id.min(recipient.org_id),
        sender.org_id.max(recipient.org_id),
        repository.transfer(sender.org_id, &actor, "same", &terms),
        repository.transfer(sender.org_id, &actor, "same", &terms),
    )
    .await?;
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

    let credentials =
        database::repositories::organization_credentials::OrganizationCredentialRepository::new(
            pool.clone(),
        );
    let credential = credentials
        .save(sender.org_id, owner, None, &credential_request(100, 100))
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
    credentials
        .revoke(
            sender.org_id,
            owner,
            serde_json::from_value(credential["id"].clone())?,
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

    let (a_result, b_result) = contended_transfers(
        &pool,
        a.org_id.min(b.org_id),
        a.org_id.max(b.org_id),
        repository.transfer(a.org_id, &a_actor, "opposite-a", &to_b),
        repository.transfer(b.org_id, &b_actor, "opposite-b", &to_a),
    )
    .await?;
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

fn credential_request(
    per_transfer: i64,
    total: i64,
) -> database::repositories::organization_credentials::CredentialRequest {
    serde_json::from_value(json!({
        "name": "Payout worker",
        "permissions": ["credits:read", "credits:transfer"],
        "constraints": {"credits:transfer": {
            "max_amount_per_transfer": per_transfer,
            "max_total_amount": total,
            "allowed_source_types": ["payment"],
        }},
        "expires_at": chrono::Utc::now() + chrono::Duration::hours(1),
    }))
    .unwrap()
}

#[tokio::test]
async fn organization_credentials_enforce_budgets_and_preserve_identity_on_rotation(
) -> anyhow::Result<()> {
    use database::repositories::organization_credentials::OrganizationCredentialRepository;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, other_owner) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 1000).await?;
    fund(&pool, sender.org_id, "staking_farm", 1000).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let credentials = OrganizationCredentialRepository::new(pool.clone());
    repository
        .policy(sender.org_id, Some((true, owner, "reviewed")))
        .await?;
    let mut config = credential_request(30, 50);
    let created = credentials
        .save(sender.org_id, owner, None, &config)
        .await?;
    let id: Uuid = serde_json::from_value(created["id"].clone())?;
    let actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            created["token"].as_str().unwrap(),
        ));

    assert_eq!(
        code(credentials.list(sender.org_id, other_owner).await),
        "credential_refused"
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "too-big",
                    &request(recipient.org_id, "payment", 31)
                )
                .await
        ),
        "credential_transfer_limit_exceeded"
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "wrong-source",
                    &request(recipient.org_id, "staking_farm", 1)
                )
                .await
        ),
        "credential_transfer_limit_exceeded"
    );

    let terms = request(recipient.org_id, "payment", 30);
    repository
        .transfer(sender.org_id, &actor, "first", &terms)
        .await?;
    let capacity = repository.capacity(sender.org_id, &actor).await?;
    assert_eq!(capacity["credential_remaining_budget"], 20);
    assert_eq!(capacity["sources"][0]["max_amount_per_transfer"], 20);
    assert!(
        repository
            .transfer(sender.org_id, &actor, "first", &terms)
            .await?
            .1
    );
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "second", &terms)
                .await
        ),
        "credential_transfer_limit_exceeded"
    );

    let rotated = credentials.rotate(sender.org_id, owner, id).await?;
    assert_eq!(rotated["id"], created["id"]);
    assert_eq!(
        code(repository.capacity(sender.org_id, &actor).await),
        "credential_refused"
    );
    let actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            rotated["token"].as_str().unwrap(),
        ));
    assert_eq!(
        repository.capacity(sender.org_id, &actor).await?["credential_remaining_budget"],
        20
    );

    // Raising the ceiling preserves the 30 already sent; lowering it below
    // usage blocks new sends without invalidating an authorized replay.
    config.constraints.transfer.max_total_amount = Some(60);
    credentials
        .save(sender.org_id, owner, Some(id), &config)
        .await?;
    assert_eq!(
        repository.capacity(sender.org_id, &actor).await?["credential_remaining_budget"],
        30
    );
    config.constraints.transfer.max_total_amount = Some(10);
    credentials
        .save(sender.org_id, owner, Some(id), &config)
        .await?;
    assert!(
        repository
            .transfer(sender.org_id, &actor, "first", &terms)
            .await?
            .1
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "new",
                    &request(recipient.org_id, "payment", 1)
                )
                .await
        ),
        "credential_transfer_limit_exceeded"
    );

    let listed = credentials.list(sender.org_id, owner).await?;
    assert_eq!(listed["credentials"][0]["transferred_amount"], "30");
    assert!(listed["credentials"][0].get("token").is_none());
    assert!(listed["credentials"][0].get("token_hash").is_none());
    assert!(!listed
        .to_string()
        .contains(created["token"].as_str().unwrap()));
    assert!(!listed
        .to_string()
        .contains(rotated["token"].as_str().unwrap()));

    credentials.revoke(sender.org_id, owner, id).await?;
    let first_revoke = credentials.list(sender.org_id, owner).await?;
    credentials.revoke(sender.org_id, owner, id).await?;
    assert_eq!(credentials.list(sender.org_id, owner).await?, first_revoke);
    assert_eq!(
        code(
            credentials
                .revoke(sender.org_id, owner, Uuid::new_v4())
                .await
        ),
        "credential_not_found"
    );
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "first", &terms)
                .await
        ),
        "credential_refused"
    );
    assert_eq!(
        repository.reconciliation(sender.org_id).await?["consistent"],
        true
    );
    assert_eq!(
        repository.reconciliation(recipient.org_id).await?["consistent"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn credential_permissions_recipient_restrictions_and_organization_ownership(
) -> anyhow::Result<()> {
    use database::repositories::organization_credentials::OrganizationCredentialRepository;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, second_owner) = organization(&pool).await?;
    let (other_recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 1000).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let credentials = OrganizationCredentialRepository::new(pool.clone());
    repository
        .policy(sender.org_id, Some((true, owner, "reviewed")))
        .await?;
    let mut config = credential_request(30, 50);
    config.permissions = vec!["credits:read".into()];
    let created = credentials
        .save(sender.org_id, owner, None, &config)
        .await?;
    let id: Uuid = serde_json::from_value(created["id"].clone())?;
    let actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            created["token"].as_str().unwrap(),
        ));

    let capacity = repository.capacity(sender.org_id, &actor).await?;
    assert_eq!(capacity["credential_can_transfer"], false);
    assert_eq!(capacity["sources"][0]["max_amount_per_transfer"], 0);
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "read-only",
                    &request(recipient.org_id, "payment", 1)
                )
                .await
        ),
        "credential_refused"
    );

    config.permissions = vec!["credits:transfer".into()];
    config.constraints.transfer.allowed_recipient_ids = Some(vec![recipient.org_id]);
    credentials
        .save(sender.org_id, owner, Some(id), &config)
        .await?;
    assert_eq!(
        code(repository.capacity(sender.org_id, &actor).await),
        "credential_refused"
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "wrong-recipient",
                    &request(other_recipient.org_id, "payment", 1)
                )
                .await
        ),
        "credential_transfer_limit_exceeded"
    );

    // Service credentials survive their creator's removal. Remaining owners can
    // discover and revoke them, while the removed owner's session is refused.
    let client = pool.get().await?;
    client
        .execute(
            r#"
            INSERT INTO organization_members (organization_id, user_id, role)
            VALUES ($1, $2, 'owner')
            "#,
            &[&sender.org_id, &second_owner],
        )
        .await?;
    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&sender.org_id, &owner],
        )
        .await?;
    assert_eq!(
        code(credentials.list(sender.org_id, owner).await),
        "credential_refused"
    );
    assert_eq!(
        credentials.list(sender.org_id, second_owner).await?["credentials"][0]["id"],
        created["id"]
    );
    repository
        .transfer(
            sender.org_id,
            &actor,
            "service",
            &request(recipient.org_id, "payment", 10),
        )
        .await?;
    credentials.revoke(sender.org_id, second_owner, id).await?;
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &actor,
                    "service",
                    &request(recipient.org_id, "payment", 10)
                )
                .await
        ),
        "credential_refused"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_credential_sends_cannot_overspend_and_org_policy_caps_all_credentials(
) -> anyhow::Result<()> {
    use database::repositories::credit_transfers::{OrganizationPolicy, TransferPolicy};
    use database::repositories::organization_credentials::OrganizationCredentialRepository;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 1000).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let credentials = OrganizationCredentialRepository::new(pool.clone());
    let policy = OrganizationPolicy {
        credentials: database::repositories::credit_transfers::CredentialPolicy { enabled: true },
        credits_transfer: TransferPolicy {
            enabled: true,
            constraints: database::repositories::organization_credentials::TransferConstraints {
                max_total_amount: Some(50),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    repository
        .configure_policy(sender.org_id, Some((policy, owner, "50 total")))
        .await?;
    let created = credentials
        .save(sender.org_id, owner, None, &credential_request(30, 30))
        .await?;
    let actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            created["token"].as_str().unwrap(),
        ));
    let terms = request(recipient.org_id, "payment", 30);

    // Keep both send futures pending on the accounting lock before releasing
    // them, rather than relying only on scheduler overlap from join!.
    let mut blocker = pool.get().await?;
    let held = blocker.transaction().await?;
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&sender.org_id.min(recipient.org_id)],
    )
    .await?;
    let first = repository.transfer(sender.org_id, &actor, "a", &terms);
    let second = repository.transfer(sender.org_id, &actor, "b", &terms);
    tokio::pin!(first, second);
    tokio::select! {
        result = &mut first => panic!("send unexpectedly completed: {result:?}"),
        result = &mut second => panic!("send unexpectedly completed: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
    }
    held.commit().await?;
    let (first, second) = tokio::join!(first, second);
    assert_ne!(first.is_ok(), second.is_ok());
    // The organization limit is checked first; both ceilings prevent the second send.
    assert_eq!(
        code(if first.is_err() { first } else { second }),
        "organization_transfer_limit_exceeded"
    );

    let other = credentials
        .save(sender.org_id, owner, None, &credential_request(100, 100))
        .await?;
    let other_actor = TransferActor::Credential(
        database::repositories::credit_transfers::hash_credential(other["token"].as_str().unwrap()),
    );
    assert_eq!(
        code(
            repository
                .transfer(
                    sender.org_id,
                    &other_actor,
                    "too-much",
                    &request(recipient.org_id, "payment", 21)
                )
                .await
        ),
        "organization_transfer_limit_exceeded"
    );
    repository
        .transfer(
            sender.org_id,
            &other_actor,
            "remaining",
            &request(recipient.org_id, "payment", 20),
        )
        .await?;
    assert_eq!(
        repository.capacity(sender.org_id, &other_actor).await?["organization_remaining_budget"],
        0
    );
    assert_eq!(
        repository.reconciliation(sender.org_id).await?["consistent"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn reconciliation_reports_drift_without_repairing_it() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "reviewed")))
        .await?;
    repository
        .transfer(
            sender.org_id,
            &TransferActor::Owner(owner),
            "send",
            &request(recipient.org_id, "payment", 50),
        )
        .await?;

    let client = pool.get().await?;
    client
        .execute(
            r#"
            UPDATE organization_credit_transfer_totals SET outgoing = 49
            WHERE organization_id = $1
              AND credit_type = 'payment'
            "#,
            &[&sender.org_id],
        )
        .await?;
    client
        .execute(
            r#"
            UPDATE organization_limits_history SET spend_limit = 0
            WHERE organization_id = $1
              AND credit_type = 'payment'
              AND effective_until IS NULL
            "#,
            &[&sender.org_id],
        )
        .await?;
    let result = repository.reconciliation(sender.org_id).await?;
    let findings = result["findings"].as_array().unwrap();
    assert_eq!(result["consistent"], false);
    assert!(findings.iter().any(|f| f["kind"] == "projection_mismatch"));
    assert!(findings
        .iter()
        .any(|f| f["kind"] == "outgoing_exceeds_source"));
    let unchanged: i64 = client
        .query_one(
            r#"
            SELECT outgoing FROM organization_credit_transfer_totals
            WHERE organization_id = $1 AND credit_type = 'payment'
            "#,
            &[&sender.org_id],
        )
        .await?
        .get(0);
    assert_eq!(unchanged, 49);

    client
        .execute(
            r#"
            UPDATE organization_credit_transfer_totals SET incoming = 49
            WHERE organization_id = $1
              AND credit_type = 'transfer'
            "#,
            &[&recipient.org_id],
        )
        .await?;
    client
        .execute(
            r#"
            UPDATE organization_limits_history SET spend_limit = 49
            WHERE organization_id = $1
              AND credit_type = 'transfer'
              AND effective_until IS NULL
            "#,
            &[&recipient.org_id],
        )
        .await?;
    client
        .execute(
            r#"
            INSERT INTO organization_credit_consumption (organization_id, credit_type, amount)
            VALUES ($1, 'transfer', 51)
            "#,
            &[&recipient.org_id],
        )
        .await?;
    let result = repository.reconciliation(recipient.org_id).await?;
    let findings = result["findings"].as_array().unwrap();
    assert!(findings
        .iter()
        .any(|f| f["kind"] == "incoming_limit_mismatch"));
    assert!(findings
        .iter()
        .any(|f| f["kind"] == "consumption_exceeds_incoming"));
    Ok(())
}

#[tokio::test]
async fn policy_quota_counts_new_transfers_but_not_retries() -> anyhow::Result<()> {
    use database::repositories::credit_transfers::{OrganizationPolicy, TransferPolicy};
    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let policy = OrganizationPolicy {
        credentials: database::repositories::credit_transfers::CredentialPolicy { enabled: true },
        credits_transfer: TransferPolicy {
            enabled: true,
            max_sends_per_minute: 1,
            ..Default::default()
        },
    };
    repository
        .configure_policy(sender.org_id, Some((policy, owner, "quota")))
        .await?;
    let actor = TransferActor::Owner(owner);
    let terms = request(recipient.org_id, "payment", 1);

    repository
        .transfer(sender.org_id, &actor, "first", &terms)
        .await?;
    assert!(
        repository
            .transfer(sender.org_id, &actor, "first", &terms)
            .await?
            .1
    );
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "second", &terms)
                .await
        ),
        "transfer_rate_limited"
    );
    assert_eq!(
        repository.capacity(sender.org_id, &actor).await?["sends_remaining_in_window"],
        0
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_calls_and_snapshot_reads_do_not_wait_for_accounting_locks(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, recipient_owner) = organization(&pool).await?;
    fund(&pool, sender.org_id, "staking_farm", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "reviewed")))
        .await?;
    let terms = request(recipient.org_id, "staking_farm", 10);
    repository
        .transfer(
            sender.org_id,
            &TransferActor::Owner(owner),
            "privacy",
            &terms,
        )
        .await?;
    let incoming = repository
        .history(
            recipient.org_id,
            &TransferActor::Owner(recipient_owner),
            None,
            None,
            None,
            10,
        )
        .await?;
    assert!(incoming["transfers"][0].get("sender_snapshot").is_none());
    assert!(incoming["transfers"][0].get("staking_evidence").is_none());
    assert!(incoming["transfers"][0].get("recipient_snapshot").is_some());

    let mut blocker = pool.get().await?;
    let held = blocker.transaction().await?;
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&sender.org_id],
    )
    .await?;
    let refused = TransferActor::Credential("not-a-real-hash".into());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        repository.transfer(sender.org_id, &refused, "denied", &terms),
    )
    .await?;
    assert_eq!(code(result), "credential_refused");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        repository.capacity(sender.org_id, &TransferActor::Owner(owner)),
    )
    .await??;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        repository.history(
            sender.org_id,
            &TransferActor::Owner(owner),
            None,
            None,
            None,
            10,
        ),
    )
    .await??;
    held.rollback().await?;
    Ok(())
}

type TransferResult = anyhow::Result<(Value, bool)>;

/// Both sends must actually join the PostgreSQL lock wait graph before release.
/// Probe the higher UUID while both sends wait on the lower UUID. A sender-first
/// ordering regression holds the higher row and fails this probe deterministically.
async fn contended_transfers(
    pool: &DbPool,
    locked_org: Uuid,
    unlocked_org: Uuid,
    first: impl std::future::Future<Output = TransferResult>,
    second: impl std::future::Future<Output = TransferResult>,
) -> anyhow::Result<(TransferResult, TransferResult)> {
    let mut blocker = pool.get().await?;
    let held = blocker.transaction().await?;
    let blocker_pid: i32 = held.query_one("SELECT pg_backend_pid()", &[]).await?.get(0);
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&locked_org],
    )
    .await?;
    let observer = pool.get().await?;

    let release = async {
        tokio::time::timeout(std::time::Duration::from_millis(1500), async {
            loop {
                let waiting: i64 = observer
                    .query_one(
                        r#"
                        WITH RECURSIVE waiting(pid) AS (
                            SELECT $1::INTEGER
                            UNION
                            SELECT a.pid FROM pg_stat_activity a
                            JOIN waiting w ON w.pid = ANY(pg_blocking_pids(a.pid))
                        )
                        SELECT COUNT(*) - 1 FROM waiting
                        "#,
                        &[&blocker_pid],
                    )
                    .await?
                    .get(0);
                if waiting >= 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        // This autocommit statement releases the probe lock before the blocker.
        observer
            .query_one(
                "SELECT id FROM organizations WHERE id = $1 FOR UPDATE NOWAIT",
                &[&unlocked_org],
            )
            .await?;
        held.commit().await?;
        Ok::<_, anyhow::Error>(())
    };
    let (first, second, released) = tokio::join!(first, second, release);
    released?;
    Ok((first, second))
}

#[tokio::test]
async fn distributed_credit_backing_cannot_be_reduced_by_limit_replacement() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "reviewed")))
        .await?;

    for source in ["payment", "staking_farm"] {
        fund(&pool, sender.org_id, source, 100).await?;
        repository
            .transfer(
                sender.org_id,
                &TransferActor::Owner(owner),
                source,
                &request(recipient.org_id, source, 60),
            )
            .await?;

        let failure = fund(&pool, sender.org_id, source, 59).await.unwrap_err();
        assert!(failure.to_string().contains("already transferred"));
        assert_eq!(
            repository.reconciliation(sender.org_id).await?["consistent"],
            true
        );
        fund(&pool, sender.org_id, source, 60).await?;
    }
    Ok(())
}

#[tokio::test]
async fn payout_program_targets_are_atomic_and_count_only_tagged_sends() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 1000).await?;
    fund(&pool, sender.org_id, "staking_farm", 1000).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "payouts")))
        .await?;
    let actor = TransferActor::Owner(owner);

    repository
        .transfer(
            sender.org_id,
            &actor,
            "untagged",
            &request(recipient.org_id, "payment", 20),
        )
        .await?;
    let mut tagged = request(recipient.org_id, "payment", 30);
    tagged.payout_program = Some("rewards".into());
    repository
        .transfer(sender.org_id, &actor, "tagged", &tagged)
        .await?;

    let mut target = request(recipient.org_id, "staking_farm", 1);
    target.amount = None;
    target.recipient_cumulative = Some(100);
    target.payout_program = Some("rewards".into());
    let mut higher = target.clone();
    higher.recipient_cumulative = Some(150);
    let (first, second) = contended_transfers(
        &pool,
        sender.org_id.min(recipient.org_id),
        sender.org_id.max(recipient.org_id),
        repository.transfer(sender.org_id, &actor, "", &target),
        repository.transfer(sender.org_id, &actor, "", &higher),
    )
    .await?;
    assert_eq!(
        first?.0["amount"].as_i64().unwrap() + second?.0["amount"].as_i64().unwrap(),
        120
    );

    let (no_op, replay) = repository
        .transfer(sender.org_id, &actor, "", &target)
        .await?;
    assert!(replay);
    assert_eq!(no_op["no_op"], true);
    assert_eq!(no_op["recipient_cumulative"], 150);
    assert_eq!(no_op["amount"], 0);

    target.source_credit_type = "payment".into();

    // A different credential observes the same program total. No-op retries
    // consume no budget, but cannot bypass recipient restrictions.
    let credentials =
        database::repositories::organization_credentials::OrganizationCredentialRepository::new(
            pool.clone(),
        );
    let created = credentials
        .save(sender.org_id, owner, None, &credential_request(0, 0))
        .await?;
    let credential_actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            created["token"].as_str().unwrap(),
        ));
    assert_eq!(
        repository
            .transfer(sender.org_id, &credential_actor, "", &target)
            .await?
            .0["recipient_cumulative"],
        150
    );

    let mut restricted = credential_request(0, 0);
    restricted.constraints.transfer.allowed_recipient_ids = Some(vec![]);
    credentials
        .save(
            sender.org_id,
            owner,
            Some(serde_json::from_value(created["id"].clone())?),
            &restricted,
        )
        .await?;
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &credential_actor, "", &target)
                .await
        ),
        "credential_transfer_limit_exceeded"
    );

    target.payout_program = Some("another-program".into());
    assert_eq!(
        repository
            .transfer(sender.org_id, &actor, "", &target)
            .await?
            .0["amount"],
        100
    );
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "not-allowed", &target)
                .await
        ),
        "invalid_transfer_terms"
    );
    target.amount = Some(1);
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "", &target)
                .await
        ),
        "invalid_transfer_terms"
    );
    target.amount = None;
    target.payout_program = None;
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "", &target)
                .await
        ),
        "invalid_transfer_terms"
    );

    Ok(())
}

#[tokio::test]
async fn grant_transfers_require_explicit_admin_source_opt_in() -> anyhow::Result<()> {
    use database::repositories::credit_transfers::{OrganizationPolicy, TransferPolicy};
    use database::repositories::organization_credentials::TransferConstraints;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "grant", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "default sources")))
        .await?;
    let actor = TransferActor::Owner(owner);
    let terms = request(recipient.org_id, "grant", 25);

    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor, "grant", &terms)
                .await
        ),
        "organization_transfer_limit_exceeded"
    );

    let policy = OrganizationPolicy {
        credits_transfer: TransferPolicy {
            enabled: true,
            constraints: TransferConstraints {
                allowed_source_types: Some(vec!["grant".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    repository
        .configure_policy(
            sender.org_id,
            Some((policy, owner, "approved grant distribution")),
        )
        .await?;
    let result = repository
        .transfer(sender.org_id, &actor, "grant", &terms)
        .await?
        .0;
    assert_eq!(result["amount"], 25);
    assert_eq!(result["destination_credit_type"], "transfer");
    assert!(request(sender.org_id, "transfer", 1)
        .validate(recipient.org_id, "second-hop")
        .is_err());

    Ok(())
}

#[tokio::test]
async fn history_cursor_ties_and_direction_filters_do_not_skip_rows() -> anyhow::Result<()> {
    use database::repositories::credit_transfers::TransferHistoryOptions;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, recipient_owner) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 100).await?;
    fund(&pool, recipient.org_id, "payment", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    repository
        .policy(sender.org_id, Some((true, owner, "history")))
        .await?;
    repository
        .policy(recipient.org_id, Some((true, recipient_owner, "history")))
        .await?;
    let actor = TransferActor::Owner(owner);
    let original = repository
        .transfer(
            sender.org_id,
            &actor,
            "original",
            &request(recipient.org_id, "payment", 1),
        )
        .await?
        .0;
    repository
        .transfer(
            recipient.org_id,
            &TransferActor::Owner(recipient_owner),
            "incoming",
            &request(sender.org_id, "payment", 1),
        )
        .await?;

    // History-only fixtures share an exact timestamp to exercise the UUID
    // tiebreaker without mutating any immutable ledger row.
    let original_id: Uuid = serde_json::from_value(original["id"].clone())?;
    let client = pool.get().await?;
    for _ in 0..3 {
        let id = Uuid::new_v4();
        client
            .execute(
                r#"
            INSERT INTO organization_credit_transfers (
                id, sender_organization_id, recipient_organization_id, source_credit_type,
                amount, idempotency_key, terms, actor_id, actor_kind, credential_id,
                policy_version, source_limit_id, recipient_limit_id, staking_evidence,
                result, recipient_snapshot, created_at
            )
            SELECT $2::UUID, sender_organization_id, recipient_organization_id, source_credit_type,
                amount, $2::UUID::TEXT, terms, actor_id, actor_kind, credential_id,
                policy_version, source_limit_id, recipient_limit_id, staking_evidence,
                result || jsonb_build_object('id', $2::UUID), recipient_snapshot, created_at
            FROM organization_credit_transfers WHERE id = $1
            "#,
                &[&original_id, &id],
            )
            .await?;
    }

    let mut cursor = None;
    let mut ids = std::collections::HashSet::new();
    loop {
        let page = repository
            .history(
                sender.org_id,
                &actor,
                None,
                None,
                cursor,
                TransferHistoryOptions {
                    limit: 2,
                    direction: Some("outgoing".into()),
                },
            )
            .await?;
        let rows = page["transfers"].as_array().unwrap();
        if rows.is_empty() {
            break;
        }
        for row in rows {
            assert_eq!(row["sender_organization_id"], sender.org_id.to_string());
            assert!(ids.insert(row["id"].as_str().unwrap().to_owned()));
        }
        cursor = Some((
            serde_json::from_value(page["next_cursor"]["before_time"].clone())?,
            serde_json::from_value(page["next_cursor"]["before_id"].clone())?,
        ));
    }
    assert_eq!(ids.len(), 4);

    let incoming = repository
        .history(
            sender.org_id,
            &actor,
            None,
            None,
            None,
            TransferHistoryOptions {
                limit: 10,
                direction: Some("incoming".into()),
            },
        )
        .await?;
    assert_eq!(incoming["transfers"].as_array().unwrap().len(), 1);
    assert!(incoming["transfers"][0].get("sender_snapshot").is_none());
    assert!(incoming["transfers"][0].get("staking_evidence").is_none());

    Ok(())
}

#[tokio::test]
async fn preflight_rejects_disabled_or_restricted_senders_without_recipient_locks(
) -> anyhow::Result<()> {
    use database::repositories::credit_transfers::OrganizationPolicy;
    use database::repositories::organization_credentials::OrganizationCredentialRepository;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let credentials = OrganizationCredentialRepository::new(pool.clone());
    let actor = TransferActor::Owner(owner);
    let terms = request(recipient.org_id, "payment", 1);
    let mut blocker = pool.get().await?;
    let held = blocker.transaction().await?;
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&recipient.org_id],
    )
    .await?;

    let refused = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        repository.transfer(sender.org_id, &actor, "disabled", &terms),
    )
    .await?;
    assert_eq!(code(refused), "sender_not_enabled");

    repository
        .policy(sender.org_id, Some((true, owner, "test preflight")))
        .await?;
    let mut configuration = credential_request(10, 100);
    configuration.constraints.transfer.allowed_recipient_ids = Some(vec![]);
    let created = credentials
        .save(sender.org_id, owner, None, &configuration)
        .await?;
    let credential_actor =
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            created["token"].as_str().unwrap(),
        ));
    let refused = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        repository.transfer(sender.org_id, &credential_actor, "restricted", &terms),
    )
    .await?;
    assert_eq!(code(refused), "credential_transfer_limit_exceeded");

    let mut policy = OrganizationPolicy::default();
    policy.credits_transfer.enabled = true;
    policy.credits_transfer.constraints.allowed_source_types = Some(vec!["grant".into()]);
    repository
        .configure_policy(sender.org_id, Some((policy, owner, "restrict source")))
        .await?;
    let refused = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        repository.transfer(sender.org_id, &actor, "source", &terms),
    )
    .await?;
    assert_eq!(code(refused), "organization_transfer_limit_exceeded");
    held.rollback().await?;

    Ok(())
}

#[tokio::test]
async fn replays_require_original_credential_or_read_permission() -> anyhow::Result<()> {
    use database::repositories::organization_credentials::OrganizationCredentialRepository;

    let pool = test_pool().await?;
    let (sender, owner) = organization(&pool).await?;
    let (recipient, _) = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 100).await?;
    let repository = CreditTransferRepository::new(pool.clone(), &Default::default());
    let credentials = OrganizationCredentialRepository::new(pool.clone());
    repository
        .policy(sender.org_id, Some((true, owner, "replay isolation")))
        .await?;
    let mut configuration = credential_request(10, 100);
    configuration.permissions = vec!["credits:transfer".into()];
    let original = credentials
        .save(sender.org_id, owner, None, &configuration)
        .await?;
    let other = credentials
        .save(sender.org_id, owner, None, &configuration)
        .await?;
    let actor = |value: &Value| {
        TransferActor::Credential(database::repositories::credit_transfers::hash_credential(
            value["token"].as_str().unwrap(),
        ))
    };
    let terms = request(recipient.org_id, "payment", 5);
    let committed = repository
        .transfer(sender.org_id, &actor(&original), "retry", &terms)
        .await?
        .0;

    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor(&other), "retry", &terms)
                .await
        ),
        "credential_refused"
    );
    let mut different = terms.clone();
    different.amount = Some(6);
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor(&other), "retry", &different)
                .await
        ),
        "idempotency_conflict"
    );

    let id = serde_json::from_value(original["id"].clone())?;
    let rotated = credentials.rotate(sender.org_id, owner, id).await?;
    configuration.constraints.transfer.allowed_recipient_ids = Some(vec![]);
    configuration.constraints.transfer.max_total_amount = Some(0);
    credentials
        .save(sender.org_id, owner, Some(id), &configuration)
        .await?;
    configuration.permissions.push("credits:read".into());
    credentials
        .save(
            sender.org_id,
            owner,
            Some(serde_json::from_value(other["id"].clone())?),
            &configuration,
        )
        .await?;
    repository
        .policy(sender.org_id, Some((false, owner, "stop new sends")))
        .await?;

    // A completed-operation replay must not wait for recipient accounting.
    let mut blocker = pool.get().await?;
    let held = blocker.transaction().await?;
    held.query_one(
        "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
        &[&recipient.org_id],
    )
    .await?;
    for allowed in [actor(&rotated), actor(&other), TransferActor::Owner(owner)] {
        let replayed = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            repository.transfer(sender.org_id, &allowed, "retry", &terms),
        )
        .await??;
        assert!(replayed.1);
        assert_eq!(replayed.0, committed);
    }
    held.rollback().await?;

    credentials.revoke(sender.org_id, owner, id).await?;
    assert_eq!(
        code(
            repository
                .transfer(sender.org_id, &actor(&rotated), "retry", &terms)
                .await
        ),
        "credential_refused"
    );
    Ok(())
}
