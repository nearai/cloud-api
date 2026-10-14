use database::repositories::trace_commons_credit::{
    TraceCommonsCreditError, TraceCommonsCreditRepository,
};
use uuid::Uuid;
#[path = "support/service_usage_reporting_pool.rs"]
mod service_usage_reporting_pool;

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
    let pool = service_usage_reporting_pool::test_pool().await?;
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
    let pool = service_usage_reporting_pool::test_pool().await?;
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
    let pool = service_usage_reporting_pool::test_pool().await?;
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
    let pool = service_usage_reporting_pool::test_pool().await?;
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
async fn a_raise_funds_usage_through_the_trace_commons_type() -> anyhow::Result<()> {
    let pool = service_usage_reporting_pool::test_pool().await?;
    let org = org(&pool).await?;
    let repo = TraceCommonsCreditRepository::new(pool.clone());
    repo.set_cumulative(org, 1_000, "s", None).await?;
    let client = pool.get().await?;
    // The allocation ledger accepts the new type (V95 widened its CHECK).
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
