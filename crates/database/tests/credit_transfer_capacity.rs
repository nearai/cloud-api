//! Kept cloud-api paths that read transfer rows written by cloud-platform:
//! usage allocation against effective capacity, staking limit sync and limit
//! replacement guards. Transfer rows are seeded with SQL because the transfer
//! writer no longer lives in this repository.
#[allow(dead_code)]
mod support;

use database::models::UpdateOrganizationLimitsDbRequest;
use database::repositories::{
    OrganizationLimitsRepository, OrganizationServiceUsageRepository,
    OrganizationStakingFarmSourcesRepository, RecordServiceUsageRequest,
};
use database::DbPool;
use services::staking_farm::StakingFarmRepository;
use support::{insert_org_fixture, test_pool, OrgFixture};
use uuid::Uuid;

async fn organization(pool: &DbPool) -> anyhow::Result<OrgFixture> {
    let org = insert_org_fixture(pool).await?;
    pool.get()
        .await?
        .execute(
            "INSERT INTO organization_balance (organization_id) VALUES ($1) ON CONFLICT DO NOTHING",
            &[&org.org_id],
        )
        .await?;
    Ok(org)
}

async fn fund(pool: &DbPool, org: Uuid, kind: &str, amount: i64) -> anyhow::Result<()> {
    set_limit(pool, org, kind, amount, "USD").await
}

async fn set_limit(
    pool: &DbPool,
    org: Uuid,
    kind: &str,
    amount: i64,
    currency: &str,
) -> anyhow::Result<()> {
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
                currency: currency.into(),
                changed_by: Some("test".into()),
                change_reason: Some("test posted credits".into()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    Ok(())
}

/// Writes what cloud-platform's transfer writer commits: the ledger row, the
/// sender's outgoing total, and the recipient's incoming total with its
/// `transfer` limit (the issuance trigger requires the totals first).
async fn seed_transfer(
    pool: &DbPool,
    sender: Uuid,
    recipient: Uuid,
    source: &str,
    amount: i64,
) -> anyhow::Result<()> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.execute(
        r#"
        INSERT INTO organization_credit_transfer_totals (organization_id, credit_type, outgoing)
        VALUES ($1, $2, $3)
        ON CONFLICT (organization_id, credit_type)
        DO UPDATE SET outgoing = organization_credit_transfer_totals.outgoing + EXCLUDED.outgoing
        "#,
        &[&sender, &source, &amount],
    )
    .await?;
    let incoming: i64 = tx
        .query_one(
            r#"
            INSERT INTO organization_credit_transfer_totals (organization_id, credit_type, incoming)
            VALUES ($1, 'transfer', $2)
            ON CONFLICT (organization_id, credit_type)
            DO UPDATE SET incoming = organization_credit_transfer_totals.incoming + EXCLUDED.incoming
            RETURNING incoming
            "#,
            &[&recipient, &amount],
        )
        .await?
        .get(0);
    tx.execute(
        r#"
        UPDATE organization_limits_history SET effective_until = NOW()
        WHERE organization_id = $1 AND credit_type = 'transfer' AND effective_until IS NULL
        "#,
        &[&recipient],
    )
    .await?;
    let recipient_limit: Uuid = tx
        .query_one(
            r#"
            INSERT INTO organization_limits_history
                (organization_id, spend_limit, credit_type, source, currency, effective_from)
            VALUES ($1, $2, 'transfer', 'credit_transfer', 'USD', NOW())
            RETURNING id
            "#,
            &[&recipient, &incoming],
        )
        .await?
        .get(0);
    let source_limit: Uuid = tx
        .query_one(
            r#"
            SELECT id FROM organization_limits_history
            WHERE organization_id = $1 AND credit_type = $2 AND effective_until IS NULL
            "#,
            &[&sender, &source],
        )
        .await?
        .get(0);
    tx.execute(
        r#"
        INSERT INTO organization_credit_transfers (
            id, sender_organization_id, recipient_organization_id, source_credit_type, amount,
            terms, actor_id, actor_kind, policy_version, source_limit_id, recipient_limit_id,
            result, recipient_snapshot
        ) VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, $6, 'owner_session', 1, $7, $8,
                  '{}'::jsonb, '{}'::jsonb)
        "#,
        &[
            &Uuid::new_v4(),
            &sender,
            &recipient,
            &source,
            &amount,
            &Uuid::new_v4(),
            &source_limit,
            &recipient_limit,
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// (spend_limit, transferred_out, effective_capacity) of the active limit.
async fn capacity(pool: &DbPool, org: Uuid, kind: &str) -> anyhow::Result<(i64, i64, i64)> {
    let row = pool
        .get()
        .await?
        .query_one(
            r#"
            SELECT spend_limit, transferred_out, effective_capacity
            FROM organization_credit_capacities
            WHERE organization_id = $1 AND credit_type = $2 AND effective_until IS NULL
            "#,
            &[&org, &kind],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2)))
}

#[tokio::test]
async fn service_usage_allocation_uses_effective_capacity_after_transfer() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let sender = organization(&pool).await?;
    let recipient = organization(&pool).await?;
    fund(&pool, sender.org_id, "payment", 100).await?;
    seed_transfer(&pool, sender.org_id, recipient.org_id, "payment", 80).await?;
    assert_eq!(
        capacity(&pool, sender.org_id, "payment").await?,
        (100, 80, 20)
    );

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

    // A request admitted before the distribution completes after it. Only the
    // 20 of effective capacity funds it; the rest is unfunded overage.
    let recorded = OrganizationServiceUsageRepository::new(pool.clone())
        .record_usage(&usage)
        .await?;

    assert_eq!(recorded.funded_amount, Some(20));
    assert_eq!(recorded.unfunded_amount, Some(10));
    assert_eq!(
        recorded.credit_allocations.unwrap()[0].credit_type,
        "payment"
    );

    // Raising the limit restores capacity above the transferred-out amount only.
    fund(&pool, sender.org_id, "payment", 115).await?;
    assert_eq!(
        capacity(&pool, sender.org_id, "payment").await?,
        (115, 80, 35)
    );
    Ok(())
}

#[tokio::test]
async fn staking_sync_and_source_lifecycle_never_reset_distribution() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let staking = OrganizationStakingFarmSourcesRepository::new(pool.clone());
    let sender = organization(&pool).await?;
    let recipient = organization(&pool).await?;
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
    seed_transfer(&pool, sender.org_id, recipient.org_id, "staking_farm", 30).await?;

    for issuance in [105, 105, 90] {
        staking
            .update_staking_farm_limit(sender.org_id, issuance, None)
            .await?;
        assert_eq!(
            capacity(&pool, sender.org_id, "staking_farm").await?,
            (105, 30, 75)
        );
    }

    client
        .execute(
            "UPDATE organization_staking_farm_sources SET status = 'disconnected' WHERE organization_id = $1",
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
            "UPDATE organization_staking_farm_sources SET status = 'active' WHERE organization_id = $1",
            &[&sender.org_id],
        )
        .await?;
    staking
        .update_staking_farm_limit(sender.org_id, 110, None)
        .await?;

    assert_eq!(
        capacity(&pool, sender.org_id, "staking_farm").await?,
        (110, 30, 80)
    );
    Ok(())
}

#[tokio::test]
async fn distributed_credit_backing_cannot_be_reduced_by_limit_replacement() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let sender = organization(&pool).await?;
    let recipient = organization(&pool).await?;

    for source in ["payment", "staking_farm"] {
        fund(&pool, sender.org_id, source, 100).await?;
        seed_transfer(&pool, sender.org_id, recipient.org_id, source, 60).await?;

        let failure = fund(&pool, sender.org_id, source, 59).await.unwrap_err();
        assert!(failure.to_string().contains("already transferred"));
        let failure = set_limit(&pool, sender.org_id, source, 100, "EUR")
            .await
            .unwrap_err();
        assert!(failure.to_string().contains("must remain backed by USD"));
        assert_eq!(capacity(&pool, sender.org_id, source).await?, (100, 60, 40));

        fund(&pool, sender.org_id, source, 60).await?;
        assert_eq!(capacity(&pool, sender.org_id, source).await?, (60, 60, 0));
    }
    Ok(())
}
