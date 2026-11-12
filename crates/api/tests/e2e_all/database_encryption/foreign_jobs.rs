use super::*;
use serde_json::{json, Value};

struct JobCleanup {
    pool: database::DbPool,
    ids: [Uuid; 2],
}

impl Drop for JobCleanup {
    fn drop(&mut self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        let client = self.pool.get().await.unwrap();
                        client.execute(
                            "UPDATE database_encryption_jobs SET cancel_requested_at=NOW(),status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END WHERE id=ANY($1) AND status IN ('queued','running')",
                            &[&&self.ids[..]],
                        ).await.unwrap();
                        loop {
                            let active: bool = client.query_one(
                                "SELECT EXISTS(SELECT 1 FROM database_encryption_jobs WHERE id=ANY($1) AND status IN ('queued','running'))",
                                &[&&self.ids[..]],
                            ).await.unwrap().get(0);
                            if !active { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        client.execute("DELETE FROM database_encryption_jobs WHERE id=ANY($1)", &[&&self.ids[..]]).await.unwrap();
                    }).await.expect("foreign job cleanup timeout");
                });
            });
        }));
        match result {
            Ok(()) => {}
            Err(error) if !std::thread::panicking() => std::panic::resume_unwind(error),
            Err(_) => eprintln!("foreign job cleanup failed while unwinding"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_and_endpoints_ignore_foreign_job_rows() {
    let (server, database) = setup_test_server_with_database().await;
    let foreign = Uuid::new_v4();
    let canary = Uuid::new_v4();
    let cleanup = JobCleanup {
        pool: database.pool().clone(),
        ids: [foreign, canary],
    };
    let client = database.pool().get().await.unwrap();
    let admin_id = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let scope = json!({"kind":"objects","options":{}});
    let cursor = json!({"object":"sentinel"});
    let progress = json!({"processed":37});
    client.execute(
        "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,cursor,progress,admin_actor) VALUES($1,'dry_run','queued',$2,'[\"encrypt\"]',1,1,$3,$4,$5)",
        &[&foreign,&scope,&cursor,&progress,&admin_id],
    ).await.unwrap();
    let before: Value = client
        .query_one(
            "SELECT to_jsonb(job) FROM database_encryption_jobs job WHERE id=$1",
            &[&foreign],
        )
        .await
        .unwrap()
        .get(0);
    // This later job proves a recovery pass reached the worker. No timing-only
    // assertion can accidentally pass because recovery never ran.
    client.execute(
        "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,admin_actor) VALUES($1,'dry_run','queued',$2,'[\"encrypt\"]',1,1,$3)",
        &[&canary,&json!({"fields":[{"table":"files","column":"filename"}]}),&admin_id],
    ).await.unwrap();
    drop(client);
    let state = api::database_encryption::DatabaseEncryptionState::new(database.pool().clone())
        .unwrap()
        .with_recovery_interval(std::time::Duration::from_millis(20));
    state.recover_jobs();
    let completed = wait_for_database_encryption_job(&server, &canary.to_string()).await;
    assert_eq!(completed["status"], "completed");

    let missing = Uuid::new_v4();
    for suffix in ["", "/cancel"] {
        let mut replies = Vec::new();
        for id in [foreign, missing] {
            let path = format!("/v1/admin/database-encryption/jobs/{id}{suffix}");
            let request = if suffix.is_empty() {
                server.get(&path)
            } else {
                server.post(&path)
            };
            let response = request
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(
                response.status_code(),
                if suffix.is_empty() { 404 } else { 400 }
            );
            replies.push(response.json::<Value>());
        }
        assert_eq!(
            replies[0], replies[1],
            "foreign id must behave like an unknown id"
        );
    }
    let client = database.pool().get().await.unwrap();
    let after: Value = client
        .query_one(
            "SELECT to_jsonb(job) FROM database_encryption_jobs job WHERE id=$1",
            &[&foreign],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, before,
        "status, cursor, progress and every other column must be untouched"
    );
    drop(client);
    drop(cleanup);
}

#[tokio::test]
async fn database_job_requests_reject_scope_kind() {
    let server = setup_test_server().await;
    for kind in [
        json!("objects"),
        json!(""),
        Value::Null,
        json!(0),
        json!({}),
    ] {
        for path in [
            "/v1/admin/database-encryption/jobs",
            "/v1/admin/database-encryption/verify",
        ] {
            let response = server
                .post(path)
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .json(&json!({
                    "mode":"dry_run", "actions":["encrypt"],
                    "scope":{"kind":kind,"fields":[{"table":"files","column":"filename"}]}
                }))
                .await;
            assert_eq!(response.status_code(), 400, "path={path}, kind={kind}");
            assert_eq!(
                response.json::<Value>()["error"]["message"],
                "scope.kind is not supported for database jobs"
            );
        }
    }
}
