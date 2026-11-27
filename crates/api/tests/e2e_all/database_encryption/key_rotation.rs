use super::*;
use database::field_encryption::{encrypt_with_key_id, KeyRing};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};

const DB_V1_KEY: [u8; 32] = [7; 32];
const DB_V2_KEY: [u8; 32] = [9; 32];

fn scope() -> Value {
    json!({"fields":[
        {"table":"files","column":"filename"},
        {"table":"responses","column":"metadata"},
        {"table":"response_items","column":"item"}
    ]})
}

async fn job(cleanup: &RotationCleanup<'_>, mode: &str, action: &str, delay: Option<i64>) -> Value {
    let server = cleanup.server;
    let path = if action == "verify" {
        "/v1/admin/database-encryption/verify"
    } else {
        "/v1/admin/database-encryption/jobs"
    };
    let response = server
        .post(path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "mode":mode, "actions":[action], "batch_size":2, "batch_delay_ms":delay,
            "scope":scope()
        }))
        .await;
    assert_eq!(response.status_code(), 202);
    let response = response.json::<Value>();
    let id = response["job_id"].as_str().unwrap();
    cleanup.jobs.borrow_mut().push(id.parse().unwrap());
    wait_for_database_encryption_job(server, id).await
}

struct RotationCleanup<'a> {
    server: &'a axum_test::TestServer,
    pool: database::pool::DbPool,
    jobs: RefCell<Vec<Uuid>>,
    completed: &'a Cell<bool>,
    temporary_files: [Uuid; 2],
}

fn default_ring(retain_rotation_keys: bool) -> KeyRing {
    let config = test_config();
    KeyRing::new(
        config.database_encryption_key_id,
        database::field_encryption::parse_key(&config.database_encryption_key).unwrap(),
        if retain_rotation_keys {
            vec![("db-v1".into(), DB_V1_KEY), ("db-v2".into(), DB_V2_KEY)]
        } else {
            vec![]
        },
    )
    .unwrap()
}

impl Drop for RotationCleanup<'_> {
    fn drop(&mut self) {
        // Restore the default writer synchronously even if cleanup later fails.
        // Retain every source until the field-scoped cleanup has rewrapped them.
        self.pool.set_encryption_ring(default_ring(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // This test uses a multithread runtime so PostgreSQL and job workers
            // continue while Drop waits, including during assertion unwinding.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        let ids = self.jobs.borrow().clone();
                        let client = self.pool.get().await.unwrap();
                        client.execute(
                            "UPDATE database_encryption_jobs SET cancel_requested_at=NOW(),status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END WHERE id=ANY($1) AND status IN('queued','running')",
                            &[&ids],
                        ).await.unwrap();
                        loop {
                            let active: bool = client.query_one(
                                "SELECT EXISTS(SELECT 1 FROM database_encryption_jobs WHERE id=ANY($1) AND status IN('queued','running'))",
                                &[&ids],
                            ).await.unwrap().get(0);
                            if !active { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        client.execute("DELETE FROM files WHERE id=ANY($1)", &[&&self.temporary_files[..]]).await.unwrap();
                        drop(client);
                        // Jobs can only scope fields, so restore all affected
                        // rows, including shared fixtures touched by rewrap.
                        job(self, "execute", "rewrap", None).await;
                        let verify = job(self, "verify", "verify", None).await;
                        assert_eq!(verify["progress"]["stale"], 0);
                    }).await.expect("rotation cleanup timeout");
                });
            });
        }));
        match result {
            Ok(()) => {
                // Only remove source keys after every affected row is restored.
                self.pool.set_encryption_ring(default_ring(false));
                self.completed.set(true);
            }
            Err(error) if !std::thread::panicking() => std::panic::resume_unwind(error),
            Err(_) => eprintln!(
                "rotation cleanup failed while unwinding; default writer restored, read keys retained"
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_restores_default_writer_and_read_keys_when_database_is_unavailable_during_unwind()
{
    let server = axum_test::TestServer::new(axum::Router::new());
    let pool = database::pool::DbPool::uninitialized();
    pool.set_encryption_ring(KeyRing::new("db-v2".into(), DB_V2_KEY, vec![]).unwrap());
    let completed = Cell::new(false);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = RotationCleanup {
            server: &server,
            pool: pool.clone(),
            jobs: RefCell::new(vec![]),
            completed: &completed,
            temporary_files: [Uuid::new_v4(), Uuid::new_v4()],
        };
        panic!("exercise cleanup without a database");
    }));
    assert!(panic.is_err());
    assert!(!completed.get());
    assert_eq!(
        pool.encryption_ring().unwrap().read_ids(),
        default_ring(true).read_ids()
    );
    let ring = pool.encryption_ring().unwrap();
    assert_eq!(ring.write_id(), test_config().database_encryption_key_id);
    for (key_id, key) in [("db-v1", DB_V1_KEY), ("db-v2", DB_V2_KEY)] {
        let id = Uuid::new_v4();
        let value = serde_json::from_str(
            &encrypt_with_key_id(&key, key_id, "files", "filename", id, "retained.txt").unwrap(),
        )
        .unwrap();
        assert_eq!(
            database::field_encryption::decrypt_with_ring(&value, &ring, "files", "filename", id)
                .unwrap(),
            "retained.txt"
        );
    }
}
