use super::*;
use axum::http::Method;
use database::models::AdminAccessTokenPermission;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_tokens_scan_but_cannot_mutate_and_job_kinds_are_isolated() {
    let ring = ring();
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    let (server, database) = server(storage, ring).await;
    let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let repository =
        database::repositories::AdminAccessTokenRepository::new(database.pool().clone());
    let (_, token) = repository
        .create(
            actor,
            "object rotation reads".into(),
            "test".into(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some(MOCK_USER_AGENT.into()),
            AdminAccessTokenPermission::ReadOnly,
        )
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let database_id = Uuid::new_v4();
    server.jobs.track(id);
    server.jobs.track(database_id);
    let client = database.pool().get().await.unwrap();
    for (job_id, scope, actions) in [
        (
            id,
            json!({"kind":"objects","options":{}}),
            json!(["rewrap"]),
        ),
        (
            database_id,
            json!({"fields":[{"table":"files","column":"filename"}]}),
            json!(["encrypt"]),
        ),
    ] {
        client.execute("INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor) VALUES($1,'dry_run','completed',$2,$3,1,$4)", &[&job_id,&scope,&actions,&actor]).await.unwrap();
    }
    for (method, suffix, body) in [
        (Method::GET, "keys".into(), json!({})),
        (Method::HEAD, "keys".into(), json!({})),
        (Method::POST, "scan".into(), json!({})),
        (Method::GET, format!("jobs/{id}"), json!({})),
    ] {
        let response = server
            .method(method, &format!("/v1/admin/object-encryption/{suffix}"))
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&body)
            .await;
        assert_eq!(response.status_code(), 200);
    }
    for suffix in ["jobs".into(), "verify".into(), format!("jobs/{id}/cancel")] {
        let response = server
            .post(&format!("/v1/admin/object-encryption/{suffix}"))
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({"mode":"execute","actions":["rewrap"]}))
            .await;
        assert_eq!(response.status_code(), 403);
        assert_eq!(
            response.json::<Value>()["error"]["type"],
            "insufficient_permissions"
        );
    }
    for (domain, foreign_id, cancel_status) in [("object", database_id, 404), ("database", id, 400)]
    {
        let response = server
            .get(&format!("/v1/admin/{domain}-encryption/jobs/{foreign_id}"))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 404);
        let response = server
            .post(&format!(
                "/v1/admin/{domain}-encryption/jobs/{foreign_id}/cancel"
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), cancel_status, "domain={domain}");
    }
    for job_id in [id, database_id] {
        let status: String = client
            .query_one(
                "SELECT status FROM database_encryption_jobs WHERE id=$1",
                &[&job_id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "completed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_resumes_committed_object_cursor_and_workers_select_their_own_kind() {
    let ring = ring();
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    for key in ["workspace/a", "workspace/b", "workspace/c"] {
        storage
            .seed_raw(
                key,
                encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                None,
                "text/plain",
            )
            .unwrap();
    }
    storage
        .rewrap_object("workspace/a", 268_435_456)
        .await
        .unwrap();
    let (server, database) = server(storage.clone(), ring.clone()).await;
    let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let object_id = Uuid::new_v4();
    let database_id = Uuid::new_v4();
    server.jobs.track(object_id);
    server.jobs.track(database_id);
    let client = database.pool().get().await.unwrap();
    client.execute("INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor,cursor,progress) VALUES($1,'execute','running',$2,'[\"rewrap\"]',1,$3,$4,$5)", &[
        &object_id,&json!({"kind":"objects","options":{}}),&actor,
        &json!({"continuation":null,"last_key":"workspace/a","complete":false,"pending_keys":["workspace/b","workspace/c"],"pending_next":null}),
        &json!({"processed":1,"total":1,"rewrapped":1}),
    ]).await.unwrap();
    client.execute("INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor) VALUES($1,'dry_run','queued',$2,'[\"encrypt\"]',1,$3)", &[
        &database_id,&json!({"fields":[{"table":"files","column":"filename"}]}),&actor,
    ]).await.unwrap();
    drop(client);

    // The object worker resumes the persisted page, leaving the database job queued.
    ObjectEncryptionState::new(database.pool().clone(), storage.clone(), ring)
        .with_job_scoped_leases()
        .with_lease_retry_interval(Duration::from_millis(20))
        .recover_job(object_id);
    let result = wait(&server, &object_id.to_string()).await;
    assert_eq!(result["progress"]["rewrapped"], 3);
    assert_eq!(result["progress"]["processed"], 3);
    assert_eq!(result["cursor"]["last_key"], "workspace/c");
    let client = database.pool().get().await.unwrap();
    let status: String = client
        .query_one(
            "SELECT status FROM database_encryption_jobs WHERE id=$1",
            &[&database_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "queued");
    // Recovery is scoped to this test-owned object job and never scans shared jobs.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_is_observed_between_batches_without_rewriting_the_remainder() {
    let ring = ring();
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    for index in 0..4 {
        storage
            .seed_raw(
                &format!("workspace/{index}"),
                encrypt_with_key(b"legacy", &[7; 32]).unwrap(),
                None,
                "text/plain",
            )
            .unwrap();
    }
    let (server, _) = server(storage.clone(), ring).await;
    let response = post(
        &server,
        "jobs",
        json!({"mode":"execute","actions":["rewrap"],"batch_size":1,"batch_delay_ms":10000}),
    )
    .await;
    assert_eq!(response.status_code(), 202);
    let id = response.json::<Value>()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = server
                .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            if response.json::<Value>()["progress"]["processed"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let response = post(&server, &format!("jobs/{id}/cancel"), json!({})).await;
    assert_eq!(response.status_code(), 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = server
                .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            let result = response.json::<Value>();
            if result["status"] == "cancelled" {
                assert_eq!(result["progress"]["processed"], 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        storage
            .object_key_id("workspace/0")
            .await
            .unwrap()
            .as_deref(),
        Some("s3-v2")
    );
    for index in 1..4 {
        assert_eq!(
            storage
                .object_key_id(&format!("workspace/{index}"))
                .await
                .unwrap(),
            None
        );
    }
}
