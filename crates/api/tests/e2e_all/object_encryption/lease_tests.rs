use super::*;
use api::object_encryption::lease::{self, Claim, Job};
use futures::FutureExt;

struct Fixture {
    state: ObjectEncryptionState,
    pool: database::DbPool,
    ids: Vec<Uuid>,
    _cleanup: EncryptionJobsCleanup,
}

impl Fixture {
    async fn new(options: &[Value]) -> Self {
        let pool = db_setup::create_test_pool().await;
        let ring = ring();
        let state = ObjectEncryptionState::new(
            pool.clone(),
            Arc::new(MockStorage::new_with_ring(ring.clone())),
            ring,
        );
        let cleanup = EncryptionJobsCleanup::new(pool.clone());
        let ids: Vec<_> = options.iter().map(|_| Uuid::new_v4()).collect();
        let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
        for (id, options) in ids.iter().zip(options) {
            // Arm cleanup before each INSERT, including partial fixture setup.
            cleanup.track(*id);
            pool.get().await.unwrap().execute(
                "INSERT INTO database_encryption_jobs(id,mode,status,scope,batch_size,actions,admin_actor) VALUES($1,'execute','queued',$2,100,'[\"rewrap\"]',$3)",
                &[id, &json!({"kind":"objects","options":options}), &actor],
            ).await.unwrap();
        }
        Self {
            state,
            pool,
            ids,
            _cleanup: cleanup,
        }
    }

    async fn row(&self, id: Uuid) -> Value {
        self.pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT to_jsonb(j) FROM database_encryption_jobs j WHERE id=$1::uuid",
                &[&id],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn owned(&self, id: Uuid) -> Box<Job> {
        let state = self.state.clone().with_job_scoped_leases();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match lease::claim(&state, id).await.unwrap() {
                    Claim::Owned(job) => return job,
                    Claim::Busy => tokio::time::sleep(Duration::from_millis(10)).await,
                    Claim::Stopped => panic!("queued fixture was stopped"),
                }
            }
        })
        .await
        .expect("lease claim timeout")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_lease_is_reclaimed_and_stale_writes_are_fenced() {
    // Given a live owner, then an expired lease on this fixture's job.
    let fixture = Fixture::new(&[json!({})]).await;
    let id = fixture.ids[0];
    let mut first = fixture.owned(id).await;
    let previous = fixture.row(id).await;
    assert!(matches!(
        lease::claim(&fixture.state.clone().with_job_scoped_leases(), id)
            .await
            .unwrap(),
        Claim::Busy
    ));
    fixture.pool.get().await.unwrap().execute(
        "UPDATE database_encryption_jobs SET cursor=jsonb_set(cursor,'{lease,expires_at}','0') WHERE id=$1::uuid", &[&id],
    ).await.unwrap();
    let second = fixture.owned(id).await;
    let owner = fixture.row(id).await;
    assert_ne!(
        owner["cursor"]["lease"]["token"],
        previous["cursor"]["lease"]["token"]
    );
    assert_eq!(owner["scope"], previous["scope"]);

    // When the expired owner attempts both a checkpoint and a failure.
    assert!(lease::persist(&fixture.state, &mut first, "completed")
        .await
        .is_err());
    assert!(!lease::fail(&fixture.state, &first).await.unwrap());
    let stale_token = previous["cursor"]["lease"]["token"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(lease::check(&fixture.state, id, stale_token).await.is_err());

    // Then every column in the new owner's row remains unchanged.
    assert_eq!(fixture.row(id).await, owner);
    // The current owner can still heartbeat, checkpoint and fail its own job.
    let token = owner["cursor"]["lease"]["token"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(!lease::check(&fixture.state, id, token).await.unwrap());
    let mut second = second;
    lease::persist(&fixture.state, &mut second, "running")
        .await
        .unwrap();
    assert!(lease::fail(&fixture.state, &second).await.unwrap());
    let failed = fixture.row(id).await;
    assert_eq!(failed["status"], "failed");
    assert!(failed["cursor"].get("lease").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_leases_cover_the_bucket_and_test_leases_cover_only_the_job() {
    // Given one live lease, claimed with job scope so unrelated tests cannot block setup.
    // Distinct, valid options satisfy the real active-scope unique index.
    let fixture = Fixture::new(&[json!({"batch_delay_ms":0}), json!({"batch_delay_ms":1})]).await;
    let first = fixture.ids[0];
    let second = fixture.ids[1];
    fixture.owned(first).await;

    // When production scope considers another job, the bucket is busy.
    assert!(matches!(
        lease::claim(&fixture.state, second).await.unwrap(),
        Claim::Busy
    ));

    // Then job scope permits that different job while fencing the same job.
    fixture.owned(second).await;
    assert!(matches!(
        lease::claim(&fixture.state.clone().with_job_scoped_leases(), second)
            .await
            .unwrap(),
        Claim::Busy
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_gate_marks_a_queued_execute_failed_without_a_lease() {
    // Given a queued execute job and a disabled write gate.
    let fixture = Fixture::new(&[json!({})]).await;
    let id = fixture.ids[0];
    let state = fixture
        .state
        .clone()
        .with_job_scoped_leases()
        .with_job_options(256, false);

    // When recovery considers the job (retry only advisory-lock contention).
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match lease::claim(&state, id).await.unwrap() {
                Claim::Stopped => break,
                Claim::Busy => tokio::time::sleep(Duration::from_millis(10)).await,
                Claim::Owned(_) => panic!("disabled write gate assigned a lease"),
            }
        }
    })
    .await
    .expect("disabled gate claim timeout");

    // Then failure is committed without assigning a lease.
    let row = fixture.row(id).await;
    assert_eq!(row["status"], "failed");
    assert!(row["cursor"].get("lease").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_cleanup_removes_jobs_on_success_and_panic() {
    let pool = db_setup::create_test_pool().await;
    for unwind in [false, true] {
        let id = Uuid::new_v4();
        let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
        let outcome = std::panic::AssertUnwindSafe(async {
            let cleanup = EncryptionJobsCleanup::new(pool.clone());
            cleanup.track(id);
            pool.get().await.unwrap().execute(
                "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor) VALUES($1,'execute','queued',$2,'[\"rewrap\"]',1,$3)",
                &[&id, &json!({"kind":"objects","options":{}}), &actor],
            ).await.unwrap();
            assert!(!unwind, "exercise fixture cleanup during assertion unwinding");
        }).catch_unwind().await;
        assert_eq!(outcome.is_err(), unwind);
        let exists: bool = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM database_encryption_jobs WHERE id=$1)",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!exists, "cleanup left its active job behind");
    }
}
