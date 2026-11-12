use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_scope_returns_conflict_without_changing_the_existing_object_job() {
    let ring = ring();
    let (server, database) = server(Arc::new(MockStorage::new_with_ring(ring.clone())), ring).await;
    let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let scope = json!({"kind":"objects","options":{"batch_delay_ms":7}});
    for status in ["queued", "running"] {
        let id = Uuid::new_v4();
        server.jobs.track(id);
        let client = database.pool().get().await.unwrap();
        client.execute(
            "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor) VALUES($1,'dry_run',$2,$3,'[\"rewrap\"]',1,$4)",
            &[&id, &status, &scope, &actor],
        ).await.unwrap();
        let before: Value = client
            .query_one(
                "SELECT to_jsonb(j) FROM database_encryption_jobs j WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);

        for (suffix, body) in [
            ("jobs", json!({"mode":"dry_run","batch_delay_ms":7})),
            ("verify", json!({"batch_delay_ms":7})),
        ] {
            let response = post(&server, suffix, body).await;
            assert_eq!(response.status_code(), 409, "existing status: {status}");
            assert_eq!(response.json::<Value>()["error"]["code"], "conflict");
            assert_eq!(
                response.json::<Value>()["error"]["message"],
                "an object encryption job is already queued or running"
            );
        }
        let after: Value = client
            .query_one(
                "SELECT to_jsonb(j) FROM database_encryption_jobs j WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(after, before);
        let active: i64 = client.query_one(
            "SELECT COUNT(*) FROM database_encryption_jobs WHERE scope=$1 AND status IN ('queued','running')", &[&scope],
        ).await.unwrap().get(0);
        assert_eq!(active, 1);
        client.execute("UPDATE database_encryption_jobs SET status='completed',completed_at=NOW() WHERE id=$1", &[&id]).await.unwrap();
        drop(client);

        let accepted = post(
            &server,
            "jobs",
            json!({"mode":"dry_run","batch_delay_ms":7}),
        )
        .await;
        assert_eq!(accepted.status_code(), 202);
        wait(
            &server,
            accepted.json::<Value>()["job_id"].as_str().unwrap(),
        )
        .await;
    }
}
