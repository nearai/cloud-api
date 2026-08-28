use crate::common::*;
use uuid::Uuid;

#[tokio::test]
async fn execute_job_is_rejected_until_encrypted_writes_are_enabled() {
    let (server, _) = setup_test_server_with_config_and_database(|config| {
        config.database_encryption_write_enabled = false;
    })
    .await;

    let response = server
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

    assert_eq!(response.status_code(), 400);
    assert_eq!(
        response.json::<serde_json::Value>()["error"]["message"],
        "execute jobs require DB_ENCRYPTION_WRITE_ENABLED=true"
    );
}

#[tokio::test]
async fn create_job_rejects_invalid_request_parameters() {
    let server = setup_test_server().await;
    let cases = [
        serde_json::json!({"mode":"dry_run","scope":{},"batch_size":1,"actions":["encrypt"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"unknown"}]},"batch_size":1,"actions":["encrypt"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"filename"}]},"batch_size":0,"actions":["encrypt"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"filename"}]},"batch_size":1001,"actions":["encrypt"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"filename"}]},"batch_size":1,"actions":["verify_only"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"filename"}]},"batch_size":1,"actions":["encrypt","encrypt"]}),
        serde_json::json!({"mode":"dry_run","scope":{"fields":[{"table":"files","column":"filename"}]},"batch_size":1,"max_rows":0,"actions":["encrypt"]}),
    ];

    for request in cases {
        let response = server
            .post("/v1/admin/database-encryption/jobs")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&request)
            .await;
        assert_eq!(response.status_code(), 400, "request: {request}");
        assert!(response.json::<serde_json::Value>()["error"]["message"]
            .as_str()
            .is_some());
    }
}

#[tokio::test]
async fn job_lifecycle_rejects_missing_and_terminal_jobs() {
    let server = setup_test_server().await;
    let missing = Uuid::new_v4();
    let get = server
        .get(format!("/v1/admin/database-encryption/jobs/{missing}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get.status_code(), 404);
    let cancel = server
        .post(format!("/v1/admin/database-encryption/jobs/{missing}/cancel").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(cancel.status_code(), 400);

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode":"dry_run",
            "scope":{"fields":[{"table":"files","column":"filename"}]},
            "batch_size":1,
            "actions":["encrypt"]
        }))
        .await;
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_database_encryption_job(&server, &job_id).await;
    let cancel = server
        .post(format!("/v1/admin/database-encryption/jobs/{job_id}/cancel").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(cancel.status_code(), 400);
}

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

async fn wait_for_database_encryption_job_status(
    server: &axum_test::TestServer,
    job_id: &str,
    expected: &str,
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
            if body["status"] == expected {
                break body;
            }
            assert_ne!(body["status"], "failed", "job failed: {body}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job status timeout")
}

#[tokio::test]
async fn execute_job_honors_max_rows_across_batches() {
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
    let file_id = Uuid::from_u128(1);
    let untouched_file_id = Uuid::from_u128(2);
    client
        .execute(
            "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,'legacy secret.txt',1,'text/plain','assistants','legacy/key',$2,$3)",
            &[&file_id, &workspace_id, &api_key_id],
        )
        .await
        .expect("legacy plaintext file");
    client
        .execute(
            "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,'later secret.txt',1,'text/plain','assistants','later/key',$2,$3)",
            &[&untouched_file_id, &workspace_id, &api_key_id],
        )
        .await
        .expect("later plaintext file");
    drop(client);

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "execute",
            "scope": {"fields": [{"table": "files", "column": "filename"}]},
            "batch_size": 1,
            "max_rows": 1,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();

    let completed = wait_for_database_encryption_job(&server, &job_id).await;
    assert_eq!(completed["progress"]["processed"], 1);
    assert_eq!(completed["progress"]["encrypted"], 1);
    assert!(completed["cursor"]["field_index"].as_u64().is_some());

    let client = database.pool().get().await.expect("database connection");
    let stored: String = client
        .query_one("SELECT filename FROM files WHERE id=$1", &[&file_id])
        .await
        .expect("stored encrypted file")
        .get(0);
    assert!(stored.contains(database::field_encryption::MARKER));
    assert!(!stored.contains("legacy secret.txt"));
    let untouched: String = client
        .query_one(
            "SELECT filename FROM files WHERE id=$1",
            &[&untouched_file_id],
        )
        .await
        .expect("untouched later file")
        .get(0);
    assert_eq!(untouched, "later secret.txt");
}

#[tokio::test]
async fn execute_job_encrypts_user_metadata_that_collides_with_the_marker() {
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
    let response_id = Uuid::new_v4();
    let metadata = serde_json::json!({
        database::field_encryption::MARKER: false,
        "private": "legacy metadata"
    });
    client
        .execute(
            "INSERT INTO responses(id,workspace_id,api_key_id,model,status,metadata) VALUES($1,$2,$3,'marker-collision-test','completed',$4)",
            &[&response_id, &workspace_id, &api_key_id, &metadata],
        )
        .await
        .expect("marker collision response fixture");
    drop(client);

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "execute",
            "scope": {"fields": [{"table": "responses", "column": "metadata"}]},
            "batch_size": 1000,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();
    wait_for_database_encryption_job(&server, &job_id).await;

    let client = database.pool().get().await.expect("database connection");
    let stored: serde_json::Value = client
        .query_one(
            "SELECT metadata FROM responses WHERE id=$1",
            &[&response_id],
        )
        .await
        .expect("encrypted marker collision metadata")
        .get(0);
    assert!(database::field_encryption::is_envelope(&stored));
    let key = database
        .pool()
        .encryption_key()
        .expect("database encryption key");
    assert_eq!(
        database::field_encryption::decrypt_json_if_encrypted(
            &key,
            "responses",
            "metadata",
            response_id,
            stored,
        )
        .expect("decrypt marker collision metadata"),
        metadata
    );
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

#[tokio::test]
async fn scan_and_verify_classify_plaintext_valid_and_invalid_envelopes() {
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
    let plaintext_id = Uuid::new_v4();
    let encrypted_id = Uuid::new_v4();
    let malformed_id = Uuid::new_v4();
    let key = database
        .pool()
        .encryption_key()
        .expect("database encryption key");
    let valid = database::field_encryption::encrypt(
        &key,
        "responses",
        "instructions",
        encrypted_id,
        "valid encrypted instructions",
    )
    .expect("valid envelope");
    let malformed = serde_json::json!({
        database::field_encryption::MARKER: true,
        "version": 1,
        "alg": "AES-256-GCM",
        "key_id": database::field_encryption::KEY_ID,
        "nonce": "invalid",
        "ciphertext": "invalid"
    })
    .to_string();
    for (id, instructions) in [
        (plaintext_id, "legacy plaintext instructions".to_string()),
        (encrypted_id, valid),
        (malformed_id, malformed),
    ] {
        client
            .execute(
                "INSERT INTO responses(id,workspace_id,api_key_id,model,status,instructions,metadata) VALUES($1,$2,$3,'encryption-test','completed',$4,'{}'::jsonb)",
                &[&id, &workspace_id, &api_key_id, &instructions],
            )
            .await
            .expect("response fixture");
    }

    let scope = serde_json::json!({
        "fields": [{"table": "responses", "column": "instructions"}]
    });
    let scan = server
        .post("/v1/admin/database-encryption/scan")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({"scope": scope}))
        .await;
    assert_eq!(scan.status_code(), 200);
    let scan = scan.json::<serde_json::Value>();
    assert!(scan["totals"]["plaintext"].as_i64().unwrap_or(0) >= 1);
    assert!(scan["totals"]["encrypted"].as_i64().unwrap_or(0) >= 1);
    assert!(scan["totals"]["invalid_envelope"].as_i64().unwrap_or(0) >= 1);
    assert_eq!(scan["totals"]["complete"], true);

    let verify = server
        .post("/v1/admin/database-encryption/verify")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({"scope": scope}))
        .await;
    assert_eq!(verify.status_code(), 200);
    let verify = verify.json::<serde_json::Value>();
    assert_eq!(verify["pass"], false);
    assert!(verify["failing_fields"]
        .as_array()
        .expect("failing fields")
        .iter()
        .any(|field| field["reason_code"] == "invalid_envelope"));

    for (id, plaintext) in [
        (plaintext_id, "legacy plaintext instructions"),
        (malformed_id, "recovered malformed instructions"),
    ] {
        let repaired =
            database::field_encryption::encrypt(&key, "responses", "instructions", id, plaintext)
                .expect("replacement envelope");
        client
            .execute(
                "UPDATE responses SET instructions=$2 WHERE id=$1",
                &[&id, &repaired],
            )
            .await
            .expect("repair response fixture");
    }

    let verify = server
        .post("/v1/admin/database-encryption/verify")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({"scope": scope}))
        .await;
    assert_eq!(verify.status_code(), 200);
    let verify = verify.json::<serde_json::Value>();
    assert_eq!(verify["pass"], true, "verification response: {verify}");
    assert!(verify["failing_fields"]
        .as_array()
        .expect("failing fields")
        .is_empty());
}

#[tokio::test]
async fn cancellation_is_observed_while_a_multi_batch_job_waits_on_a_row() {
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
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    for (id, filename) in [
        (first_id, "cancel-first.txt"),
        (second_id, "cancel-second.txt"),
    ] {
        client
            .execute(
                "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,$2,1,'text/plain','assistants','cancel/key',$3,$4)",
                &[&id, &filename, &workspace_id, &api_key_id],
            )
            .await
            .expect("cancellation file fixture");
    }

    let transaction = client.transaction().await.expect("locking transaction");
    transaction
        .query_one("SELECT id FROM files WHERE id=$1 FOR UPDATE", &[&first_id])
        .await
        .expect("locked first batch row");

    let create = server
        .post("/v1/admin/database-encryption/jobs")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "mode": "execute",
            "scope": {"fields": [
                {"table": "files", "column": "filename"},
                {"table": "files", "column": "storage_key"}
            ]},
            "batch_size": 1,
            "actions": ["encrypt"]
        }))
        .await;
    assert_eq!(create.status_code(), 202);
    let job_id = create.json::<serde_json::Value>()["job_id"]
        .as_str()
        .expect("job ID")
        .to_string();
    wait_for_database_encryption_job_status(&server, &job_id, "running").await;

    let cancel = server
        .post(format!("/v1/admin/database-encryption/jobs/{job_id}/cancel").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(cancel.status_code(), 200);
    transaction.commit().await.expect("unlock first batch row");

    let cancelled = wait_for_database_encryption_job_status(&server, &job_id, "cancelled").await;
    assert_eq!(cancelled["progress"]["processed"], 1);
    let second_storage_key: String = client
        .query_one("SELECT storage_key FROM files WHERE id=$1", &[&second_id])
        .await
        .expect("second cancellation fixture")
        .get(0);
    assert_eq!(second_storage_key, "cancel/key");
}

#[tokio::test]
async fn recovery_resumes_the_cursor_and_duplicate_workers_do_not_double_process() {
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
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    for (id, metadata) in [
        (first_id, serde_json::json!({"legacy": "first"})),
        (second_id, serde_json::json!({"legacy": "second"})),
    ] {
        client
            .execute(
                "INSERT INTO responses(id,workspace_id,api_key_id,model,status,metadata) VALUES($1,$2,$3,'recovery-test','completed',$4)",
                &[&id, &workspace_id, &api_key_id, &metadata],
            )
            .await
            .expect("recovery response fixture");
    }

    let job_id = Uuid::new_v4();
    let scope = serde_json::json!({
        "tables": [],
        "fields": [{"table": "responses", "column": "metadata"}]
    });
    let actions = serde_json::json!(["encrypt"]);
    let cursor = serde_json::json!({"field_index": 0, "after_id": first_id});
    let progress = serde_json::json!({"processed": 1, "encrypted": 0});
    let max_rows = 2_i64;
    let batch_size = 1_i64;
    let admin_id = Uuid::parse_str(MOCK_USER_ID).expect("admin UUID");
    client
        .execute(
            "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,cursor,progress,admin_actor) VALUES($1,'execute','queued',$2,$3,$4,$5,$6,$7,$8)",
            &[
                &job_id,
                &scope,
                &actions,
                &batch_size,
                &max_rows,
                &cursor,
                &progress,
                &admin_id,
            ],
        )
        .await
        .expect("persisted recovery job");

    let transaction = client.transaction().await.expect("locking transaction");
    transaction
        .query_one(
            "SELECT id FROM responses WHERE id=$1 FOR UPDATE",
            &[&second_id],
        )
        .await
        .expect("locked resumed row");
    let key = database
        .pool()
        .encryption_key()
        .expect("database encryption key");
    let state = api::database_encryption::DatabaseEncryptionState::new(
        database.pool().clone(),
        &hex::encode(key),
    )
    .expect("database encryption state");
    state.recover_jobs();
    wait_for_database_encryption_job_status(&server, &job_id.to_string(), "running").await;
    state.recover_jobs();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    transaction.commit().await.expect("unlock resumed row");

    let completed = wait_for_database_encryption_job(&server, &job_id.to_string()).await;
    assert_eq!(completed["progress"]["processed"], 2);
    assert_eq!(completed["progress"]["encrypted"], 1);
    let stored: serde_json::Value = client
        .query_one("SELECT metadata FROM responses WHERE id=$1", &[&second_id])
        .await
        .expect("resumed response")
        .get(0);
    assert_eq!(stored[database::field_encryption::MARKER], true);
}
