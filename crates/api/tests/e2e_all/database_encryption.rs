use crate::common::*;
use uuid::Uuid;

async fn wait_for_database_encryption_job(
    server: &axum_test::TestServer,
    job_id: &str,
) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = server
                .get(format!("/v1/admin/database-encryption/jobs/{job_id}").as_str())
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(status.status_code(), 200);
            let body = status.json::<serde_json::Value>();
            match body["status"].as_str() {
                Some("completed") => break body,
                Some("failed") | Some("cancelled") => panic!("job did not complete: {body}"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("job completion timeout")
}

#[tokio::test]
async fn execute_job_encrypts_plaintext_and_reports_durable_progress() {
    let (server, database) = setup_test_server_with_database().await;
    let organization = create_org(&server).await;
    let _api_key = get_api_key_for_org(&server, organization.id.clone()).await;
    let organization_id = Uuid::parse_str(&organization.id).expect("organization UUID");
    let client = database.pool().get().await.expect("database connection");
    let workspace_id: Uuid = client
        .query_one(
            "SELECT id FROM workspaces WHERE organization_id=$1 LIMIT 1",
            &[&organization_id],
        )
        .await
        .expect("workspace")
        .get(0);
    let api_key_id: Uuid = client
        .query_one(
            "SELECT id FROM api_keys WHERE workspace_id=$1 LIMIT 1",
            &[&workspace_id],
        )
        .await
        .expect("API key")
        .get(0);
    let file_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,'legacy secret.txt',1,'text/plain','assistants','legacy/key',$2,$3)",
            &[&file_id, &workspace_id, &api_key_id],
        )
        .await
        .expect("legacy plaintext file");
    drop(client);

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "execute",
            "scope": {"fields": [{"table": "files", "column": "filename"}]},
            "batch_size": 1,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();

    let completed = wait_for_database_encryption_job(&server, &job_id).await;
    assert!(completed["progress"]["processed"].as_i64().unwrap_or(0) >= 1);
    assert!(completed["progress"]["encrypted"].as_i64().unwrap_or(0) >= 1);
    assert!(completed["cursor"]["field_index"].as_u64().is_some());

    let client = database.pool().get().await.expect("database connection");
    let stored: String = client
        .query_one("SELECT filename FROM files WHERE id=$1", &[&file_id])
        .await
        .expect("stored encrypted file")
        .get(0);
    assert!(stored.contains(database::field_encryption::MARKER));
    assert!(!stored.contains("legacy secret.txt"));
}

#[tokio::test]
async fn dry_run_pages_through_every_plaintext_row() {
    let (server, database) = setup_test_server_with_database().await;
    let organization = create_org(&server).await;
    let _api_key = get_api_key_for_org(&server, organization.id.clone()).await;
    let organization_id = Uuid::parse_str(&organization.id).expect("organization UUID");
    let client = database.pool().get().await.expect("database connection");
    let workspace_id: Uuid = client
        .query_one(
            "SELECT id FROM workspaces WHERE organization_id=$1 LIMIT 1",
            &[&organization_id],
        )
        .await
        .expect("workspace")
        .get(0);
    let api_key_id: Uuid = client
        .query_one(
            "SELECT id FROM api_keys WHERE workspace_id=$1 LIMIT 1",
            &[&workspace_id],
        )
        .await
        .expect("API key")
        .get(0);

    let first_file_id = Uuid::new_v4();
    let second_file_id = Uuid::new_v4();
    for (id, storage_key) in [
        (first_file_id, "first/legacy/key"),
        (second_file_id, "second/legacy/key"),
    ] {
        client
            .execute(
                "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,'legacy.txt',1,'text/plain','assistants',$2,$3,$4)",
                &[&id, &storage_key, &workspace_id, &api_key_id],
            )
            .await
            .expect("legacy plaintext file");
    }
    drop(client);

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "dry_run",
            "scope": {"fields": [{"table": "files", "column": "storage_key"}]},
            "batch_size": 1,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();

    let completed = wait_for_database_encryption_job(&server, &job_id).await;
    assert!(completed["progress"]["processed"].as_i64().unwrap_or(0) >= 2);
    assert_eq!(completed["progress"]["encrypted"], 0);

    let client = database.pool().get().await.expect("database connection");
    let plaintext: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM files WHERE id IN ($1,$2) AND storage_key NOT LIKE '%__near_db_encrypted%'",
            &[&first_file_id, &second_file_id],
        )
        .await
        .expect("plaintext files")
        .get(0);
    assert_eq!(plaintext, 2);
}

#[tokio::test]
async fn execute_job_waits_for_locked_rows_instead_of_skipping_them() {
    let (server, database) = setup_test_server_with_database().await;
    let organization = create_org(&server).await;
    let _api_key = get_api_key_for_org(&server, organization.id.clone()).await;
    let organization_id = Uuid::parse_str(&organization.id).expect("organization UUID");
    let mut client = database.pool().get().await.expect("database connection");
    let workspace_id: Uuid = client
        .query_one(
            "SELECT id FROM workspaces WHERE organization_id=$1 LIMIT 1",
            &[&organization_id],
        )
        .await
        .expect("workspace")
        .get(0);
    let api_key_id: Uuid = client
        .query_one(
            "SELECT id FROM api_keys WHERE workspace_id=$1 LIMIT 1",
            &[&workspace_id],
        )
        .await
        .expect("API key")
        .get(0);
    let file_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,'locked legacy secret.txt',1,'text/plain','assistants','legacy/key',$2,$3)",
            &[&file_id, &workspace_id, &api_key_id],
        )
        .await
        .expect("legacy plaintext file");

    let transaction = client.transaction().await.expect("locking transaction");
    transaction
        .query_one("SELECT id FROM files WHERE id=$1 FOR UPDATE", &[&file_id])
        .await
        .expect("locked file");

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "execute",
            "scope": {"fields": [{"table": "files", "column": "content_type"}]},
            "batch_size": 1,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let blocked = server
        .get(format!("/v1/admin/database-encryption/jobs/{job_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(blocked.status_code(), 200);
    assert!(matches!(
        blocked.json::<serde_json::Value>()["status"].as_str(),
        Some("queued" | "running")
    ));

    transaction.commit().await.expect("unlock file");
    let completed = wait_for_database_encryption_job(&server, &job_id).await;
    assert!(completed["progress"]["processed"].as_i64().unwrap_or(0) >= 1);
    assert!(completed["progress"]["encrypted"].as_i64().unwrap_or(0) >= 1);

    let stored: String = client
        .query_one("SELECT content_type FROM files WHERE id=$1", &[&file_id])
        .await
        .expect("stored encrypted file")
        .get(0);
    assert!(stored.contains(database::field_encryption::MARKER));
    assert!(!stored.contains("text/plain"));
}
