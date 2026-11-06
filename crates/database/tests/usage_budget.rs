//! Cloud API's SQL adapter must reserve before dispatch and recover durable metadata.
#[allow(dead_code)]
mod support;

use database::models::UpdateOrganizationLimitsDbRequest;
use database::repositories::{OrganizationLimitsRepository, OrganizationUsageRepository};
use services::usage::budget::{BudgetReservation, UsageBounds};
use services::usage::{BillingIdentity, ModelPricing, UsageError, UsageRepository};
use std::sync::Arc;
use support::{insert_model, insert_org_fixture, test_pool};
use uuid::Uuid;

#[tokio::test]
async fn concurrent_adapters_reserve_once_and_recover_after_pool_loss() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "budget-adapter").await?;
    OrganizationLimitsRepository::new(pool.clone())
        .update_limits(
            org.org_id,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: 1000,
                credit_type: "payment".into(),
                currency: "USD".into(),
                source: None,
                changed_by: None,
                change_reason: None,
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    let pricing = ModelPricing {
        id: model.id,
        model_name: model.name.clone(),
        input_cost_per_token: 10,
        output_cost_per_token: 20,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
    };
    let base = BudgetReservation {
        id: Uuid::new_v4(),
        identity: BillingIdentity {
            organization_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model_id: model.id,
        },
        amount: 400,
        pricing,
        bounds: UsageBounds::text(20, 5, 1)?,
    };
    let repository = Arc::new(OrganizationUsageRepository::new(pool.clone()));
    let barrier = Arc::new(tokio::sync::Barrier::new(10));
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..10 {
        let repo = repository.clone();
        let barrier = barrier.clone();
        let mut reservation = base.clone();
        reservation.id = Uuid::new_v4();
        if i % 2 == 1 {
            reservation.identity.workspace_id = org.workspace_b_id;
            reservation.identity.api_key_id = org.api_key_b_id;
        }
        tasks.spawn(async move {
            barrier.wait().await;
            (
                reservation.clone(),
                repo.reserve_and_dispatch(&reservation).await,
            )
        });
    }
    let mut admitted = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (reservation, result) = result?;
        match result {
            Ok(()) => admitted.push(reservation),
            Err(UsageError::LimitExceeded(_)) => {}
            other => panic!("unexpected admission result: {other:?}"),
        }
    }
    assert_eq!(admitted.len(), 2);
    assert!(
        repository.reserve_and_dispatch(&admitted[0]).await.is_err(),
        "a replayed admission must not claim another execution"
    );
    let row = pool.get().await?.query_one(
        "SELECT reserved_credit($1), count(*) FROM usage_budget_reservations WHERE organization_id=$1 AND state='dispatched'",
        &[&org.org_id]).await?;
    assert_eq!(row.get::<_, i64>(0), 800);
    assert_eq!(row.get::<_, i64>(1), 2);
    drop(repository);
    drop(pool);

    // A replacement process sees the hold even if the original one lost all in-memory state.
    let pool = test_pool().await?;
    let recovered = OrganizationUsageRepository::new(pool.clone());
    let snapshot = recovered
        .budget_snapshot(org.org_id, admitted[0].id)
        .await?
        .unwrap();
    assert_eq!(snapshot.pricing.input_cost_per_token, 10);
    assert_eq!(snapshot.bounds, base.bounds);
    assert!(matches!(
        recovered.reserve_and_dispatch(&base).await,
        Err(UsageError::LimitExceeded(_))
    ));
    let row = pool
        .get()
        .await?
        .query_one(
            "SELECT release_unused_budget($1,$2)",
            &[&org.org_id, &admitted[0].id],
        )
        .await?;
    assert!(
        !row.get::<_, bool>(0),
        "a crashed dispatched attempt cannot be refunded"
    );
    Ok(())
}
