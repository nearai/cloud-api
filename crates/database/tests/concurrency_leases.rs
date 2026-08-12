mod support;

use database::repositories::concurrency_lease::PostgresConcurrencyLeaseRepository;
use services::completions::ports::{ConcurrencyLeaseRepository, LeaseOutcome};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const LIMIT: i32 = 3;
const ATTEMPTS: usize = 24;
const TTL: Duration = Duration::from_secs(30);
const DEFAULT_LIMIT: u32 = 64;

/// Every replica runs this same read-then-insert, so without the advisory lock
/// they all read the same count before any of them has inserted and the limit
/// is exceeded by however many arrive together.
#[tokio::test]
async fn concurrent_acquires_never_exceed_the_limit() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    pool.get()
        .await
        .expect("connection")
        .execute(
            "UPDATE organizations SET rate_limit = $1 WHERE id = $2",
            &[&LIMIT, &org.org_id],
        )
        .await
        .expect("rate limit update");

    let repository = Arc::new(PostgresConcurrencyLeaseRepository::new(pool.clone()));

    let attempts: Vec<_> = (0..ATTEMPTS)
        .map(|attempt| {
            let repository = repository.clone();
            let org_id = org.org_id;
            let model_id = model.id;
            tokio::spawn(async move {
                repository
                    .try_acquire(
                        Uuid::new_v4(),
                        org_id,
                        model_id,
                        &format!("instance-{}", attempt % 4),
                        DEFAULT_LIMIT,
                        TTL,
                    )
                    .await
            })
        })
        .collect();

    let mut admitted = 0usize;
    let mut at_limit = 0usize;
    for attempt in attempts {
        match attempt.await.expect("task").expect("acquire") {
            LeaseOutcome::Admitted => admitted += 1,
            LeaseOutcome::AtLimit { .. } => at_limit += 1,
        }
    }

    assert_eq!(
        admitted, LIMIT as usize,
        "{ATTEMPTS} racing acquires admitted {admitted}, limit is {LIMIT}"
    );
    assert_eq!(at_limit, ATTEMPTS - LIMIT as usize);

    let live: i64 = pool
        .get()
        .await
        .expect("connection")
        .query_one(
            "SELECT COUNT(*) FROM concurrency_leases WHERE organization_id = $1",
            &[&org.org_id],
        )
        .await
        .expect("lease count")
        .get(0);
    assert_eq!(live, i64::from(LIMIT), "rows written must match admissions");
}

#[tokio::test]
async fn released_leases_free_capacity_again() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    pool.get()
        .await
        .expect("connection")
        .execute(
            "UPDATE organizations SET rate_limit = $1 WHERE id = $2",
            &[&LIMIT, &org.org_id],
        )
        .await
        .expect("rate limit update");

    let repository = PostgresConcurrencyLeaseRepository::new(pool.clone());
    let mut held = Vec::new();
    for _ in 0..LIMIT {
        let lease_id = Uuid::new_v4();
        let outcome = repository
            .try_acquire(
                lease_id,
                org.org_id,
                model.id,
                "instance-a",
                DEFAULT_LIMIT,
                TTL,
            )
            .await
            .expect("acquire");
        assert_eq!(outcome, LeaseOutcome::Admitted);
        held.push(lease_id);
    }

    let blocked = repository
        .try_acquire(
            Uuid::new_v4(),
            org.org_id,
            model.id,
            "instance-b",
            DEFAULT_LIMIT,
            TTL,
        )
        .await
        .expect("acquire");
    assert!(matches!(blocked, LeaseOutcome::AtLimit { .. }));

    repository.release(&held[..1]).await.expect("release");

    let readmitted = repository
        .try_acquire(
            Uuid::new_v4(),
            org.org_id,
            model.id,
            "instance-b",
            DEFAULT_LIMIT,
            TTL,
        )
        .await
        .expect("acquire");
    assert_eq!(
        readmitted,
        LeaseOutcome::Admitted,
        "releasing a lease must free exactly one slot"
    );
}

/// An expired lease must not keep counting, or a replica that died holding
/// leases would lock the organization out permanently.
#[tokio::test]
async fn expired_leases_stop_counting() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    pool.get()
        .await
        .expect("connection")
        .execute(
            "UPDATE organizations SET rate_limit = 1 WHERE id = $1",
            &[&org.org_id],
        )
        .await
        .expect("rate limit update");

    let repository = PostgresConcurrencyLeaseRepository::new(pool.clone());
    let outcome = repository
        .try_acquire(
            Uuid::new_v4(),
            org.org_id,
            model.id,
            "instance-dead",
            DEFAULT_LIMIT,
            Duration::from_secs(1),
        )
        .await
        .expect("acquire");
    assert_eq!(outcome, LeaseOutcome::Admitted);

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let after_expiry = repository
        .try_acquire(
            Uuid::new_v4(),
            org.org_id,
            model.id,
            "instance-live",
            DEFAULT_LIMIT,
            TTL,
        )
        .await
        .expect("acquire");
    assert_eq!(
        after_expiry,
        LeaseOutcome::Admitted,
        "a lease past its TTL must not hold capacity"
    );
}
