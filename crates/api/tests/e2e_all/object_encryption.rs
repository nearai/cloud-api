use crate::common::*;
use api::object_encryption::ObjectEncryptionState;
use axum_test::TestServer;
use serde_json::{json, Value};
use services::files::{
    encryption::{encrypt_with_key, ObjectKeyRing},
    storage::{MockStorage, StorageTrait},
};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

fn ring() -> Arc<ObjectKeyRing> {
    Arc::new(ObjectKeyRing::new("s3-v2".into(), [9; 32], vec![("s3-v1".into(), [7; 32])]).unwrap())
}

async fn server(
    storage: Arc<MockStorage>,
    ring: Arc<ObjectKeyRing>,
) -> (TestServer, Arc<database::Database>) {
    setup_test_server_with_domain_services(move |services, database| {
        services.files_service = Arc::new(services::files::FileServiceImpl::new(
            Arc::new(database::repositories::FileRepository::new(
                database.pool().clone(),
            )),
            storage.clone(),
        ));
        services.object_encryption_state =
            ObjectEncryptionState::new(database.pool().clone(), storage, ring);
    })
    .await
}

async fn post(server: &TestServer, suffix: &str, body: Value) -> axum_test::TestResponse {
    server
        .post(&format!("/v1/admin/object-encryption/{suffix}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn wait(server: &TestServer, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = server
                .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(response.status_code(), 200);
            let body = response.json::<Value>();
            match body["status"].as_str() {
                Some("completed") => return body,
                Some("failed" | "cancelled") => panic!("object job failed: {body}"),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("object job completion timeout")
}

async fn job(server: &TestServer, mode: &str) -> Value {
    let response = post(
        server,
        "jobs",
        json!({"mode":mode,"actions":["rewrap"],"batch_size":1}),
    )
    .await;
    assert_eq!(response.status_code(), 202);
    wait(server, response.json::<Value>()["job_id"].as_str().unwrap()).await
}

async fn content(server: &TestServer, file_id: &str, key: &str) {
    let response = server
        .get(&format!("/v1/files/{file_id}/content"))
        .add_header("Authorization", format!("Bearer {key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.as_bytes(), b"retained content".as_slice());
}

#[tokio::test]
async fn rotation_retains_file_reads_and_verifies_after_previous_key_removal() {
    let ring = ring();
    let storage = Arc::new(MockStorage::new_with_ring(ring.clone()));
    let (server, database) = server(storage.clone(), ring.clone()).await;
    let (api_key, owner) = create_org_and_api_key(&server).await;
    let repository = database::repositories::FileRepository::new(database.pool().clone());
    let mut files = Vec::new();
    for _ in 0..3 {
        let storage_key = format!("{}/{}", owner.workspace_id, Uuid::new_v4());
        storage
            .seed_raw(
                &storage_key,
                encrypt_with_key(b"retained content", &[7; 32]).unwrap(),
                None,
                "text/plain",
            )
            .unwrap();
        let file = repository
            .create(services::files::CreateFileParams {
                filename: "rotation.txt".into(),
                bytes: 16,
                content_type: "text/plain".into(),
                purpose: "user_data".into(),
                storage_key: storage_key.clone(),
                workspace_id: owner.workspace_id.parse().unwrap(),
                uploaded_by_api_key_id: owner.id.parse().unwrap(),
                expires_at: None,
            })
            .await
            .unwrap();
        files.push((format!("file-{}", file.id), storage_key));
    }
    content(&server, &files[0].0, &api_key).await;
    let scan = post(&server, "scan", json!({})).await;
    assert_eq!(scan.status_code(), 200);
    assert_eq!(scan.json::<Value>()["totals"]["by_key_id"]["none"], 3);
    let dry_run = job(&server, "dry_run").await;
    assert_eq!(dry_run["progress"]["total"], 3);
    for (_, key) in &files {
        assert_eq!(storage.object_key_id(key).await.unwrap(), None);
    }
    let execute = job(&server, "execute").await;
    assert_eq!(execute["progress"]["rewrapped"], 3);
    assert_eq!(execute["scope"]["kind"], "objects");
    let second = job(&server, "execute").await;
    assert_eq!(second["progress"]["rewrapped"], 0);
    assert_eq!(second["progress"]["already_current"], 3);
    let verification = post(&server, "verify", json!({"batch_size":1})).await;
    assert_eq!(verification.status_code(), 202);
    let result = wait(
        &server,
        verification.json::<Value>()["job_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(result["progress"]["pass"], true);

    let write_only = Arc::new(ObjectKeyRing::new("s3-v2".into(), [9; 32], vec![]).unwrap());
    let write_storage = Arc::new(storage.with_key_ring(write_only.clone()));
    let (new_server, _) = self::server(write_storage, write_only).await;
    for (file_id, _) in &files {
        content(&new_server, file_id, &api_key).await;
    }
    let verification = post(&new_server, "verify", json!({})).await;
    let result = wait(
        &new_server,
        verification.json::<Value>()["job_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(result["progress"]["pass"], true);

    storage
        .seed_raw(
            "unknown-key",
            encrypt_with_key(b"unknown", &[3; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    let result = job(&server, "execute").await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["progress"]["undecryptable"], 1);
    assert_eq!(
        result["progress"]["undecryptable_keys"],
        json!(["unknown-key"])
    );
    let verification = post(&server, "verify", json!({})).await;
    let result = wait(
        &server,
        verification.json::<Value>()["job_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(result["progress"]["pass"], false);
}

#[path = "object_encryption/lifecycle.rs"]
mod lifecycle;
