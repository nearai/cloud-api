#[allow(dead_code)]
mod support;

use database::models::{RecordUsageRequest, UpdateOrganizationLimitsDbRequest};
use database::repositories::credit_transfers::{
    CreditTransferRepository, TransferActor, TransferRequest,
};
use database::repositories::trace_commons_credit::{
    TraceCommonsCreditError, TraceCommonsCreditRepository,
};
use database::repositories::{
    OrganizationLimitsRepository, OrganizationServiceUsageRepository, OrganizationUsageRepository,
    RecordServiceUsageRequest,
};
use support::{insert_model, insert_org_fixture, test_pool, ModelFixture, OrgFixture};
use uuid::Uuid;

async fn org(pool: &database::DbPool) -> anyhow::Result<Uuid> {
    let id = Uuid::new_v4();
    pool.get()
        .await?
        .execute(
            "INSERT INTO organizations(id,name) VALUES($1,$2)",
            &[&id, &format!("trace-commons-{id}")],
        )
        .await?;
    Ok(id)
}

async fn rows(pool: &database::DbPool, org: Uuid) -> anyhow::Result<Vec<(i64, bool)>> {
    Ok(pool
        .get()
        .await?
        .query(
            "SELECT spend_limit, effective_until IS NULL FROM organization_limits_history
              WHERE organization_id = $1 AND credit_type = 'trace_commons'
              ORDER BY effective_from",
            &[&org],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
}

#[tokio::test]
async fn cumulative_writes_are_monotonic_and_idempotent() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());

    assert_eq!(repo.get_cumulative(org).await?, Some(0));
    let mut seen = Vec::new();
    for amount in [100, 100, 50, 250, 0, 250] {
        let w = repo.set_cumulative(org, amount, "sha256:ab", None).await?;
        seen.push((w.cumulative_nano_usd, w.applied));
    }
    assert_eq!(
        seen,
        [
            (100, true),
            (100, false),
            (100, false),
            (250, true),
            (250, false),
            (250, false)
        ]
    );
    assert_eq!(rows(&pool, org).await?, [(100, false), (250, true)]);
    assert_eq!(repo.get_cumulative(org).await?, Some(250));
    Ok(())
}

#[tokio::test]
async fn concurrent_writers_converge_on_the_maximum() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    let mut tasks = Vec::new();
    for amount in [10i64, 70, 30, 70, 20, 60] {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            repo.set_cumulative(org, amount, "s", None).await
        }));
    }
    for t in tasks {
        t.await??;
    }
    assert_eq!(repo.get_cumulative(org).await?, Some(70));
    let history = rows(&pool, org).await?;
    assert_eq!(history.iter().filter(|(_, active)| *active).count(), 1);
    // Every recorded raise is strictly larger than the one before it.
    assert!(history.windows(2).all(|w| w[0].0 < w[1].0));
    Ok(())
}

#[tokio::test]
async fn an_inactive_or_unknown_org_is_not_written() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    pool.get()
        .await?
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&org],
        )
        .await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    for target in [org, Uuid::new_v4()] {
        let err = repo.set_cumulative(target, 5, "s", None).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<TraceCommonsCreditError>(),
            Some(&TraceCommonsCreditError::NotFound)
        );
        assert_eq!(repo.get_cumulative(target).await?, None);
    }
    assert!(rows(&pool, org).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn invalid_amounts_and_references_are_refused() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    for (amount, reference) in [
        (-1, "s"),
        (1_000_000_000_000_000_001, "s"),
        (1, ""),
        (1, "has space"),
    ] {
        assert!(matches!(
            repo.set_cumulative(org, amount, reference, None)
                .await
                .unwrap_err()
                .downcast_ref::<TraceCommonsCreditError>(),
            Some(TraceCommonsCreditError::Invalid(_))
        ));
    }
    Ok(())
}

#[tokio::test]
async fn the_generic_limit_writer_refuses_trace_commons() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    repo.set_cumulative(org, 500, "s", None).await?;
    let refused = OrganizationLimitsRepository::new(pool.clone())
        .update_limits(
            org,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: 1,
                credit_type: "trace_commons".into(),
                source: Some("trace-commons".into()),
                currency: "USD".into(),
                changed_by: Some("test".into()),
                change_reason: Some("lowering attempt".into()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await;
    assert!(refused.is_err());
    assert_eq!(repo.get_cumulative(org).await?, Some(500));
    assert_eq!(rows(&pool, org).await?, [(500, true)]);
    Ok(())
}

#[tokio::test]
async fn a_raise_funds_usage_through_the_trace_commons_type() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    repo.set_cumulative(org, 1_000, "s", None).await?;
    let client = pool.get().await?;
    // The allocation ledger accepts the new type (V0096 widened its CHECK).
    client
        .execute(
            "INSERT INTO organization_credit_consumption (organization_id, credit_type, amount)
             VALUES ($1, 'trace_commons', 400)",
            &[&org],
        )
        .await?;
    let available: i64 = client
        .query_one(
            "SELECT olh.spend_limit - occ.amount FROM organization_limits_history olh
               JOIN organization_credit_consumption occ
                 ON occ.organization_id = olh.organization_id AND occ.credit_type = olh.credit_type
              WHERE olh.organization_id = $1 AND olh.credit_type = 'trace_commons'
                AND olh.effective_until IS NULL",
            &[&org],
        )
        .await?
        .get(0);
    assert_eq!(available, 600);
    Ok(())
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

async fn set_limit(
    pool: &database::DbPool,
    org: Uuid,
    credit_type: &str,
    amount: i64,
) -> anyhow::Result<()> {
    OrganizationLimitsRepository::new(pool.clone())
        .update_limits(
            org,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: amount,
                credit_type: credit_type.into(),
                source: Some(if credit_type == "staking_farm" {
                    "house-of-stake".into()
                } else {
                    format!("test-{credit_type}")
                }),
                currency: "USD".into(),
                changed_by: Some("test".into()),
                change_reason: Some("test ceiling".into()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    Ok(())
}

/// Give `org` 2 grant, 3 staking_farm, 4 trace_commons, 5 transfer (received
/// from a freshly funded reseller) and 6 payment, plus 100 postpay.
async fn fund_all_six(pool: &database::DbPool, org: &OrgFixture) -> anyhow::Result<()> {
    for (credit_type, amount) in [
        ("grant", 2),
        ("staking_farm", 3),
        ("payment", 6),
        ("postpay", 100),
    ] {
        set_limit(pool, org.org_id, credit_type, amount).await?;
    }
    TraceCommonsCreditRepository::new(pool.clone())
        .set_cumulative(org.org_id, 4, "six-type-order", None)
        .await?;

    let reseller = insert_org_fixture(pool).await?;
    let client = pool.get().await?;
    let owner: Uuid = client
        .query_one(
            "SELECT created_by_user_id FROM workspaces WHERE id = $1",
            &[&reseller.workspace_a_id],
        )
        .await?
        .get(0);
    client
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&reseller.org_id, &owner],
        )
        .await?;
    set_limit(pool, reseller.org_id, "payment", 5).await?;
    let transfers = CreditTransferRepository::new(pool.clone(), &Default::default());
    transfers
        .policy(reseller.org_id, Some((true, owner, "Reviewed reseller")))
        .await?;
    transfers
        .transfer(
            reseller.org_id,
            &TransferActor::Owner(owner),
            "six-type-order",
            &TransferRequest {
                recipient_organization_id: org.org_id,
                source_credit_type: "payment".into(),
                amount: 5,
                memo: String::new(),
                metadata: Default::default(),
            },
        )
        .await?;
    Ok(())
}

/// The default v3 order is grant -> staking_farm -> trace_commons -> transfer
/// -> payment -> postpay, and the inference SQL function (V0095, not redefined
/// by V0096) and the Rust service allocator agree on it.
#[tokio::test]
async fn default_order_spends_trace_commons_after_staking_and_before_transfer() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let model = insert_model(&pool, "trace-commons-six-type-order").await?;
    let inference_org = insert_org_fixture(&pool).await?;
    let service_org = insert_org_fixture(&pool).await?;
    fund_all_six(&pool, &inference_org).await?;
    fund_all_six(&pool, &service_org).await?;

    // 2 + 3 + 4 + 5 + 6 = 20 prepaid, then 1 from postpay.
    let cost = 21;
    let expected = vec![
        ("grant".to_string(), 2),
        ("staking_farm".to_string(), 3),
        ("trace_commons".to_string(), 4),
        ("transfer".to_string(), 5),
        ("payment".to_string(), 6),
        ("postpay".to_string(), 1),
    ];

    let inference = OrganizationUsageRepository::new(pool.clone())
        .record_usage(usage(&inference_org, &model, cost))
        .await?;
    let service_id = Uuid::new_v4();
    pool.get()
        .await?
        .execute(
            "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, 'Six-type order', 'request', $3)",
            &[&service_id, &format!("trace-commons-order-{service_id}"), &cost],
        )
        .await?;
    let service = OrganizationServiceUsageRepository::new(pool.clone())
        .record_usage(&RecordServiceUsageRequest {
            organization_id: service_org.org_id,
            workspace_id: service_org.workspace_a_id,
            api_key_id: service_org.api_key_a_id,
            service_id,
            quantity: 1,
            total_cost: cost,
            inference_id: Some(Uuid::new_v4()),
        })
        .await?;

    for allocations in [inference.credit_allocations, service.credit_allocations] {
        let allocations = allocations.unwrap();
        assert!(allocations.iter().all(|a| a.policy_version == "v3"));
        assert_eq!(
            allocations
                .into_iter()
                .map(|a| (a.credit_type, a.amount))
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(inference.unfunded_amount, Some(0));
    assert_eq!(service.unfunded_amount, Some(0));

    // Fixtures stay: the transfer ledger is immutable and references the orgs.
    Ok(())
}
