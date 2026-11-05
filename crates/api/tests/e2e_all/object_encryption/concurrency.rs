use super::{paused_storage::PausedStorage, *};

async fn get_job(server: &TestServer, id: &str) -> Value {
    let response = server
        .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    response.json()
}

async fn submit_large_batch(server: &ObjectTestServer) -> String {
    let response = post(
        server,
        "jobs",
        json!({"mode":"execute","actions":["rewrap"],"batch_size":1000}),
    )
    .await;
    assert_eq!(response.status_code(), 202);
    response.json::<Value>()["job_id"].as_str().unwrap().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_storage_io_finishes_only_the_current_object() {
    let ring = ring();
    let storage = PausedStorage::new(ring.clone(), 4, 1);
    let (server, _) = server(storage.clone(), ring).await;
    let id = submit_large_batch(&server).await;
    storage.wait_until_paused().await;

    let cancellation = tokio::time::timeout(
        Duration::from_secs(1),
        post(&server, &format!("jobs/{id}/cancel"), json!({})),
    )
    .await;
    storage.release.notify_one();
    let response = cancellation.expect("cancel endpoint blocked behind storage I/O");
    assert_eq!(response.status_code(), 200);
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = get_job(&server, &id).await;
            match result["status"].as_str() {
                Some("cancelled") => break result,
                Some("failed" | "completed") => panic!("unexpected terminal job: {result}"),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("worker did not observe cancellation between objects");
    assert_eq!(result["progress"]["processed"], 1);
    assert_eq!(result["progress"]["rewrapped"], 1);
    assert_eq!(
        storage.inner.object_key_id("workspace/0000").await.unwrap(),
        Some("s3-v2".into())
    );
    for index in 1..4 {
        assert_eq!(
            storage
                .inner
                .object_key_id(&format!("workspace/{index:04}"))
                .await
                .unwrap(),
            None
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_batch_persists_a_checkpoint_before_the_fifty_first_object_finishes() {
    let ring = ring();
    let storage = PausedStorage::new(ring.clone(), 75, 51);
    let (server, database) = server(storage.clone(), ring).await;
    let id = submit_large_batch(&server).await;
    storage.wait_until_paused().await;
    let checkpoint = tokio::time::timeout(Duration::from_secs(1), get_job(&server, &id)).await;
    let persisted = tokio::time::timeout(Duration::from_secs(1), async {
        let client = database.pool().get().await.unwrap();
        client
            .query_one(
                "SELECT progress,cursor FROM database_encryption_jobs WHERE id=$1",
                &[&Uuid::parse_str(&id).unwrap()],
            )
            .await
            .unwrap()
    })
    .await;
    storage.release.notify_one();

    let checkpoint = checkpoint.expect("checkpoint lookup blocked behind storage I/O");
    assert_eq!(checkpoint["status"], "running");
    assert_eq!(checkpoint["progress"]["processed"], 50);
    let persisted = persisted.expect("checkpoint database read timed out");
    let progress: Value = persisted.get("progress");
    let cursor: Value = persisted.get("cursor");
    assert_eq!(progress["processed"], 50);
    assert_eq!(cursor["last_key"], "workspace/0049");
    assert_eq!(cursor["pending_keys"].as_array().unwrap().len(), 25);
    let completed = wait(&server, &id).await;
    assert_eq!(completed["progress"]["processed"], 75);
    assert_eq!(completed["progress"]["rewrapped"], 75);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_retries_a_busy_job_lease_without_waiting_for_global_recovery() {
    let ring = ring();
    let storage = PausedStorage::new(ring.clone(), 1, 1);
    let (server, database) = server(storage.clone(), ring.clone()).await;
    let id = Uuid::new_v4();
    server.jobs.track(id);
    let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let cursor = json!({"lease":{
        "token":Uuid::new_v4(),
        "expires_at":chrono::Utc::now().timestamp() + 120
    }});
    let client = database.pool().get().await.unwrap();
    client.execute(
        "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor,cursor,progress) VALUES($1,'execute','running',$2,'[\"rewrap\"]',1000,$3,$4,'{}')",
        &[&id, &json!({"kind":"objects","options":{}}), &actor, &cursor],
    ).await.unwrap();
    drop(client);
    let state = ObjectEncryptionState::new(database.pool().clone(), storage.clone(), ring)
        .with_job_scoped_leases()
        .with_lease_retry_interval(Duration::from_millis(20));
    state.recover_job(id);

    assert!(
        tokio::time::timeout(Duration::from_millis(20), storage.entered.notified())
            .await
            .is_err(),
        "a worker processed an object while another lease was live"
    );
    assert_eq!(
        storage.inner.object_key_id("workspace/0000").await.unwrap(),
        None
    );
    let client = database.pool().get().await.unwrap();
    let released = client.query_one(
        "UPDATE database_encryption_jobs SET cursor=jsonb_set(cursor,'{lease,expires_at}','0') WHERE id=$1 RETURNING cursor->'lease'->>'token' AS token",
        &[&id],
    ).await.unwrap();
    assert_eq!(
        released.get::<_, String>("token"),
        cursor["lease"]["token"].as_str().unwrap()
    );
    drop(client);
    let resumed = tokio::time::timeout(Duration::from_secs(1), storage.entered.notified()).await;
    storage.release.notify_one();
    resumed.expect("busy lease worker did not retry promptly after expiry");
    let completed = wait(&server, &id.to_string()).await;
    assert_eq!(completed["progress"]["rewrapped"], 1);
}
