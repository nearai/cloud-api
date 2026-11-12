use super::*;
use serde_json::{json, Value};

async fn post_job(
    server: &axum_test::TestServer,
    cleanup: &EncryptionJobsCleanup,
    body: &Value,
) -> axum_test::TestResponse {
    let response = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(body)
        .await;
    if response.status_code() == 202 {
        cleanup.track(
            response.json::<Value>()["job_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
    }
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_scope_returns_conflict_without_changing_the_existing_database_job() {
    let (server, database) = setup_test_server_with_database().await;
    let cleanup = EncryptionJobsCleanup::new(database.pool().clone());
    let actor = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let scope = json!({"tables":[],"fields":[{"table":"files","column":"filename"}],"options":{"batch_delay_ms":7}});
    let body =
        json!({"mode":"dry_run","scope":scope,"actions":["encrypt"],"batch_size":1,"max_rows":1});
    for status in ["queued", "running"] {
        let id = Uuid::new_v4();
        cleanup.track(id);
        let client = database.pool().get().await.unwrap();
        client.execute(
            "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,admin_actor) VALUES($1,'dry_run',$2,$3,'[\"encrypt\"]',1,$4)",
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
        let response = post_job(&server, &cleanup, &body).await;
        assert_eq!(response.status_code(), 409, "existing status: {status}");
        assert_eq!(response.json::<Value>()["error"]["code"], "conflict");
        assert_eq!(
            response.json::<Value>()["error"]["message"],
            "a database encryption job with this scope is already queued or running"
        );
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

        let accepted = post_job(&server, &cleanup, &body).await;
        assert_eq!(accepted.status_code(), 202);
        wait_for_database_encryption_job(
            &server,
            accepted.json::<Value>()["job_id"].as_str().unwrap(),
        )
        .await;
    }
}
