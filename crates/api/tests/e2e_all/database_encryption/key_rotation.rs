use super::*;
use config::{KeySource, KeySourceSpec};
use database::field_encryption::{encrypt_json_with_key_id, encrypt_with_key_id, KeyRing};
use serde_json::{json, Value};
use services::responses::models::{ResponseId, ResponseItemId};
use services::responses::ports::{ResponseItemRepositoryTrait, ResponseRepositoryTrait};
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

fn ring_config(config: &mut config::ApiConfig) {
    config.database_encryption_keys = Some(vec![
        KeySourceSpec {
            key_id: "db-v2".into(),
            source: KeySource::Hex(hex::encode(DB_V2_KEY)),
        },
        KeySourceSpec {
            key_id: "db-v1".into(),
            source: KeySource::Hex(hex::encode(DB_V1_KEY)),
        },
        KeySourceSpec {
            key_id: config.database_encryption_key_id.clone(),
            source: KeySource::Hex(config.database_encryption_key.clone()),
        },
    ]);
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

async fn seeded_envelopes(
    pool: &database::pool::DbPool,
    file_ids: &[Uuid],
    response_and_item: (Uuid, Uuid),
) -> Vec<Value> {
    let client = pool.get().await.unwrap();
    let mut values: Vec<Value> = client
        .query(
            "SELECT filename FROM files WHERE id=ANY($1) ORDER BY id",
            &[&file_ids],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| serde_json::from_str(&row.get::<_, String>(0)).unwrap())
        .collect();
    for (query, id) in [
        (
            "SELECT metadata FROM responses WHERE id=$1",
            response_and_item.0,
        ),
        (
            "SELECT item FROM response_items WHERE id=$1",
            response_and_item.1,
        ),
    ] {
        values.push(client.query_one(query, &[&id]).await.unwrap().get(0));
    }
    assert_eq!(values.len(), file_ids.len() + 2);
    values
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_rotation_rewraps_previous_envelopes_before_key_removal() {
    let (server, database) = setup_test_server_with_config_and_database(ring_config).await;
    let cleanup_completed = Cell::new(false);
    let unknown_id = Uuid::new_v4();
    let plaintext_id = Uuid::new_v4();
    let cleanup = RotationCleanup {
        server: &server,
        pool: database.pool().clone(),
        jobs: RefCell::new(vec![]),
        completed: &cleanup_completed,
        temporary_files: [unknown_id, plaintext_id],
    };
    // Seed unrelated encrypted fields with the ordinary key; jobs below only
    // rotate the three explicit fields in scope().
    database.pool().set_encryption_ring(default_ring(true));
    let organization = create_org(&server).await;
    let _ = get_api_key_for_org(&server, organization.id.clone()).await;
    let organization_id = Uuid::parse_str(&organization.id).unwrap();
    let (conversation, item_ids) = seed_conversation_history(&database, organization_id, 1).await;
    let client = database.pool().get().await.unwrap();
    let workspace: Uuid = client
        .query_one(
            "SELECT id FROM workspaces WHERE organization_id=$1 LIMIT 1",
            &[&organization_id],
        )
        .await
        .unwrap()
        .get(0);
    let root = client
        .query_one(
            "SELECT id,is_root_response FROM responses WHERE conversation_id=$1",
            &[&conversation.id.0],
        )
        .await
        .unwrap();
    let response_id: Uuid = root.get(0);
    let root_flag_before: bool = root.get(1);
    assert!(root_flag_before);
    let config = test_config();
    database.pool().set_encryption_ring(
        KeyRing::new(
            "db-v2".into(),
            DB_V2_KEY,
            vec![
                ("db-v1".into(), DB_V1_KEY),
                (
                    config.database_encryption_key_id,
                    database::field_encryption::parse_key(&config.database_encryption_key).unwrap(),
                ),
            ],
        )
        .unwrap(),
    );
    drop(client);
    let client = database.pool().get().await.unwrap();
    let metadata = json!({"root_response":true});
    let previous_metadata = encrypt_json_with_key_id(
        &DB_V1_KEY,
        "db-v1",
        "responses",
        "metadata",
        response_id,
        &metadata,
    )
    .unwrap();
    client
        .execute(
            "UPDATE responses SET metadata=$2 WHERE id=$1",
            &[&response_id, &previous_metadata],
        )
        .await
        .unwrap();
    let item_id = ResponseItemId(item_ids[0].strip_prefix("msg_").unwrap().parse().unwrap());
    let item = database
        .response_items
        .get_by_id(item_id.clone(), conversation.workspace_id.clone())
        .await
        .unwrap()
        .unwrap();
    let item = serde_json::to_value(item).unwrap();
    assert_eq!(item["content"][0]["text"], "retained item 0");
    let previous_item = encrypt_json_with_key_id(
        &DB_V1_KEY,
        "db-v1",
        "response_items",
        "item",
        item_id.0,
        &item,
    )
    .unwrap();
    client
        .execute(
            "UPDATE response_items SET item=$2 WHERE id=$1",
            &[&item_id.0, &previous_item],
        )
        .await
        .unwrap();
    let api_key: Uuid = client
        .query_one(
            "SELECT id FROM api_keys WHERE workspace_id=$1 LIMIT 1",
            &[&workspace],
        )
        .await
        .unwrap()
        .get(0);
    let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (index, id) in ids.iter().enumerate() {
        let (key_id, key) = if index < 2 {
            ("db-v1", DB_V1_KEY)
        } else {
            ("db-v2", DB_V2_KEY)
        };
        let filename =
            encrypt_with_key_id(&key, key_id, "files", "filename", *id, "rotation.txt").unwrap();
        client.execute("INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,$2,1,'text/plain','assistants','rotation/key',$3,$4)", &[id,&filename,&workspace,&api_key]).await.unwrap();
    }
    drop(client);
    let repository = database::repositories::file::FileRepository::new(database.pool().clone());
    for id in ids {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().unwrap().filename,
            "rotation.txt"
        );
    }

    let keys = server
        .get("/v1/admin/database-encryption/keys")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(keys.status_code(), 200);
    let keys = keys.json::<Value>();
    assert_eq!(keys["write_key_id"], "db-v2");
    assert_eq!(keys["read_key_ids"][0], "db-v2");
    assert_eq!(keys["fingerprints"]["db-v1"], "820ed242bff6e2d9");
    assert_eq!(keys["fingerprints"]["db-v2"], "825674e6c3e8ca73");
    assert!(!keys.to_string().contains(&hex::encode(DB_V1_KEY)));

    let before = seeded_envelopes(database.pool(), &ids, (response_id, item_id.0)).await;
    assert_eq!(
        before
            .iter()
            .filter(|value| value["key_id"] == "db-v1")
            .count(),
        4
    );
    let dry_run = job(&cleanup, "dry_run", "rewrap", None).await;
    assert_eq!(dry_run["progress"]["rewrapped"], 0);
    let after = seeded_envelopes(database.pool(), &ids, (response_id, item_id.0)).await;
    assert_eq!(before, after);

    let started = std::time::Instant::now();
    let execute = job(&cleanup, "execute", "rewrap", Some(25)).await;
    assert_eq!(execute["scope"]["options"]["batch_delay_ms"], 25);
    let after = seeded_envelopes(database.pool(), &ids, (response_id, item_id.0)).await;
    assert!(after.iter().all(|value| value["key_id"] == "db-v2"));
    let batches = execute["progress"]["processed"]
        .as_u64()
        .unwrap()
        .div_ceil(2);
    assert!(started.elapsed() >= std::time::Duration::from_millis(batches * 25));
    assert_eq!(
        database
            .responses
            .get_by_id(ResponseId(response_id), conversation.workspace_id.clone())
            .await
            .unwrap()
            .unwrap()
            .metadata,
        Some(metadata)
    );
    assert_eq!(
        serde_json::to_value(
            database
                .response_items
                .get_by_id(item_id.clone(), conversation.workspace_id.clone())
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        item
    );
    let client = database.pool().get().await.unwrap();
    let root_flag_after: bool = client
        .query_one(
            "SELECT is_root_response FROM responses WHERE id=$1",
            &[&response_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(root_flag_after, root_flag_before);
    drop(client);
    job(&cleanup, "execute", "rewrap", None).await;
    assert_eq!(
        after,
        seeded_envelopes(database.pool(), &ids, (response_id, item_id.0)).await
    );
    let verify = job(&cleanup, "verify", "verify", None).await;
    // The API cannot scope by row ID. Shared plaintext/invalid rows may keep
    // pass=false; only absence of stale envelopes is a field-wide invariant.
    assert_eq!(verify["progress"]["stale"], 0);
    database
        .pool()
        .set_encryption_ring(KeyRing::new("db-v2".into(), DB_V2_KEY, vec![]).unwrap());
    assert_eq!(
        database.pool().encryption_ring().unwrap().read_ids(),
        vec!["db-v2"]
    );
    for id in ids {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().unwrap().filename,
            "rotation.txt"
        );
    }
    assert_eq!(
        job(&cleanup, "verify", "verify", None).await["progress"]["stale"],
        0
    );

    let unknown = encrypt_with_key_id(
        &[8; 32],
        "db-v3",
        "files",
        "filename",
        unknown_id,
        "unknown.txt",
    )
    .unwrap();
    let client = database.pool().get().await.unwrap();
    for (id, filename) in [
        (unknown_id, unknown.clone()),
        (plaintext_id, "plain.txt".into()),
    ] {
        client.execute("INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,$2,1,'text/plain','assistants','rotation/key',$3,$4)", &[&id,&filename,&workspace,&api_key]).await.unwrap();
    }
    drop(client);
    let invalid = job(&cleanup, "execute", "rewrap", None).await;
    assert_eq!(invalid["status"], "completed");
    assert!(invalid["progress"]["by_key_id"]["totals"]
        .get("db-v3")
        .is_none());
    let client = database.pool().get().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT filename FROM files WHERE id=$1", &[&unknown_id])
            .await
            .unwrap()
            .get::<_, String>(0),
        unknown
    );
    assert_eq!(
        client
            .query_one("SELECT filename FROM files WHERE id=$1", &[&plaintext_id])
            .await
            .unwrap()
            .get::<_, String>(0),
        "plain.txt"
    );
    drop(client);
    // Exercise the same Drop path used by a failed assertion, and prove it
    // completed rather than merely swallowing a second panic while unwinding.
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = cleanup;
        panic!("exercise rotation cleanup during unwinding");
    }));
    assert!(panic.is_err());
    assert!(cleanup_completed.get());
    assert_eq!(
        database.pool().encryption_ring().unwrap().read_ids(),
        vec![test_config().database_encryption_key_id.as_str()]
    );
    let restored = seeded_envelopes(database.pool(), &ids, (response_id, item_id.0)).await;
    assert!(restored
        .iter()
        .all(|value| value["key_id"] == test_config().database_encryption_key_id));
    for id in ids {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().unwrap().filename,
            "rotation.txt"
        );
    }
}
