// Shared with other test binaries, which use a different subset of it.
#[allow(dead_code)]
mod support;

use database::repositories::concurrency_lease::PostgresConcurrencyLeaseRepository;
use services::completions::ports::{ConcurrencyLeaseRepository, HeldLease, LeaseOutcome};
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
            LeaseOutcome::Admitted { .. } => admitted += 1,
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
        assert!(matches!(outcome, LeaseOutcome::Admitted { .. }));
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
    assert!(
        matches!(readmitted, LeaseOutcome::Admitted { .. }),
        "releasing a lease must free exactly one slot"
    );
}

/// Renewal is what lets a request outlive the TTL. Without it a request longer
/// than the TTL frees its own slot while still running.
#[tokio::test]
async fn renewal_keeps_a_lease_past_its_original_ttl() {
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
    let lease_id = Uuid::new_v4();
    let outcome = repository
        .try_acquire(
            lease_id,
            org.org_id,
            model.id,
            "instance-a",
            DEFAULT_LIMIT,
            Duration::from_secs(1),
        )
        .await
        .expect("acquire");
    assert!(matches!(outcome, LeaseOutcome::Admitted { .. }));

    repository.renew(&[lease_id], TTL).await.expect("renew");

    tokio::time::sleep(Duration::from_millis(1200)).await;

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
    assert!(
        matches!(blocked, LeaseOutcome::AtLimit { .. }),
        "a renewed lease must still hold its slot after the original TTL"
    );
}

/// Renewal must never recreate a row. A request that finished during the round
/// trip has already been deleted, and an insert here would strand a slot that
/// nothing holds, renews or releases.
#[tokio::test]
async fn renewal_cannot_recreate_a_released_lease() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    let repository = PostgresConcurrencyLeaseRepository::new(pool.clone());
    let lease_id = Uuid::new_v4();
    repository
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

    repository.release(&[lease_id]).await.expect("release");
    let renewed = repository.renew(&[lease_id], TTL).await.expect("renew");
    assert!(
        renewed.is_empty(),
        "a released lease must not report as renewed"
    );

    let live: i64 = pool
        .get()
        .await
        .expect("connection")
        .query_one(
            "SELECT COUNT(*) FROM concurrency_leases WHERE id = $1",
            &[&lease_id],
        )
        .await
        .expect("count")
        .get(0);
    assert_eq!(
        live, 0,
        "renewing a released lease must not bring it back to life"
    );
}

/// Leases admitted while the store was unreachable are written back, so they
/// start counting against the fleet instead of only this replica.
#[tokio::test]
async fn pending_leases_are_written_back() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    let repository = PostgresConcurrencyLeaseRepository::new(pool.clone());
    let lease_id = Uuid::new_v4();
    let pending = [HeldLease {
        id: lease_id,
        organization_id: org.org_id,
        model_id: model.id,
    }];

    repository
        .persist(&pending, "instance-a", TTL)
        .await
        .expect("persist");

    let live: i64 = pool
        .get()
        .await
        .expect("connection")
        .query_one(
            "SELECT COUNT(*) FROM concurrency_leases WHERE id = $1 AND expires_at > NOW()",
            &[&lease_id],
        )
        .await
        .expect("count")
        .get(0);
    assert_eq!(live, 1, "a degraded-path lease must reach the store");
}

/// A lost response makes `retry_db!` run the acquire again under the same id.
/// If the remaining capacity went to someone else meanwhile, the rejection has
/// to drop the row the first attempt committed; nothing else would, because the
/// caller takes the rejection and never registers the lease to release it.
#[tokio::test]
async fn a_rejected_retry_drops_the_lease_its_first_attempt_committed() {
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
    let lease_id = Uuid::new_v4();
    let already_committed = [
        HeldLease {
            id: lease_id,
            organization_id: org.org_id,
            model_id: model.id,
        },
        HeldLease {
            id: Uuid::new_v4(),
            organization_id: org.org_id,
            model_id: model.id,
        },
    ];
    repository
        .persist(&already_committed, "instance-a", TTL)
        .await
        .expect("persist");

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
    assert!(
        matches!(outcome, LeaseOutcome::AtLimit { .. }),
        "the other lease holds the only slot, got {outcome:?}"
    );

    let surviving: i64 = pool
        .get()
        .await
        .expect("connection")
        .query_one(
            "SELECT COUNT(*) FROM concurrency_leases WHERE id = $1",
            &[&lease_id],
        )
        .await
        .expect("count")
        .get(0);
    assert_eq!(
        surviving, 0,
        "a rejected acquire must not leave its lease holding capacity"
    );
}

/// A zero or negative rate_limit means unset, not a limit of zero. Reading it
/// literally would reject every request for the organization.
#[tokio::test]
async fn a_zero_rate_limit_falls_back_to_the_default() {
    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("lease-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    let repository = PostgresConcurrencyLeaseRepository::new(pool.clone());

    for stored in [0i32, -1i32] {
        pool.get()
            .await
            .expect("connection")
            .execute(
                "UPDATE organizations SET rate_limit = $1 WHERE id = $2",
                &[&stored, &org.org_id],
            )
            .await
            .expect("rate limit update");

        let outcome = repository
            .try_acquire(
                Uuid::new_v4(),
                org.org_id,
                model.id,
                "instance-a",
                DEFAULT_LIMIT,
                TTL,
            )
            .await
            .expect("acquire");

        assert!(
            matches!(outcome, LeaseOutcome::Admitted { limit } if limit == DEFAULT_LIMIT),
            "rate_limit {stored} must fall back to the default, got {outcome:?}"
        );
    }
}

/// cargo test --test concurrency_leases admission_throughput -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn admission_throughput() {
    let concurrency: usize = std::env::var("BENCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    let rounds: usize = std::env::var("BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);

    let pool = support::test_pool().await.expect("test pool");
    let org = support::insert_org_fixture(&pool)
        .await
        .expect("org fixture");
    let model = support::insert_model(&pool, &format!("bench-{}", Uuid::new_v4()))
        .await
        .expect("model fixture");

    pool.get()
        .await
        .expect("connection")
        .execute(
            "UPDATE organizations SET rate_limit = 100000 WHERE id = $1",
            &[&org.org_id],
        )
        .await
        .expect("rate limit update");

    let repository = Arc::new(PostgresConcurrencyLeaseRepository::new(pool.clone()));
    let started = std::time::Instant::now();

    for _ in 0..rounds {
        let mut batch = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let repository = repository.clone();
            let org_id = org.org_id;
            let model_id = model.id;
            batch.push(tokio::spawn(async move {
                let lease_id = Uuid::new_v4();
                repository
                    .try_acquire(lease_id, org_id, model_id, "bench", DEFAULT_LIMIT, TTL)
                    .await
                    .expect("acquire");
                lease_id
            }));
        }
        let mut ids: Vec<Uuid> = Vec::with_capacity(concurrency);
        for handle in batch {
            ids.push(handle.await.expect("task"));
        }
        repository.release(&ids).await.expect("release");
    }

    let elapsed = started.elapsed();
    let total = rounds * concurrency;
    println!(
        "concurrency={concurrency} admissions: {total} in {elapsed:?} => {:.0}/s, {:.2}ms mean",
        total as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / total as f64
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
    assert!(matches!(outcome, LeaseOutcome::Admitted { .. }));

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
    assert!(
        matches!(after_expiry, LeaseOutcome::Admitted { .. }),
        "a lease past its TTL must not hold capacity"
    );
}
