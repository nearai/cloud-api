use super::*;
use config::{KeySource, KeySourceSpec};
use database::field_encryption::{encrypt_json_with_key_id, encrypt_with_key_id, KeyRing};
use serde_json::{json, Value};
use services::responses::models::{ResponseId, ResponseItemId};
use services::responses::ports::{ResponseItemRepositoryTrait, ResponseRepositoryTrait};

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
            source: KeySource::Hex("09".repeat(32)),
        },
        KeySourceSpec {
            key_id: "db-v1".into(),
            source: KeySource::Hex("07".repeat(32)),
        },
        KeySourceSpec {
            key_id: config.database_encryption_key_id.clone(),
            source: KeySource::Hex(config.database_encryption_key.clone()),
        },
    ]);
}

async fn job(
    server: &axum_test::TestServer,
    mode: &str,
    action: &str,
    delay: Option<i64>,
) -> Value {
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
    wait_for_database_encryption_job(server, response["job_id"].as_str().unwrap()).await
}

#[tokio::test]
async fn key_rotation_rewraps_previous_envelopes_before_key_removal() {
    let (server, database) = setup_test_server_with_config_and_database(ring_config).await;
    // Bring pre-existing fixtures to the write key before measuring this test's rows.
    job(&server, "execute", "encrypt", None).await;
    let initial = job(&server, "execute", "rewrap", None).await;
    assert_eq!(initial["progress"]["invalid_envelopes"], 0);
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
    let metadata = json!({"root_response":true});
    let previous_metadata = encrypt_json_with_key_id(
        &[7; 32],
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
        &[7; 32],
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
            ("db-v1", [7; 32])
        } else {
            ("db-v2", [9; 32])
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
    assert_eq!(keys["fingerprints"]["db-v1"].as_str().unwrap().len(), 16);
    assert!(!keys.to_string().contains(&"07".repeat(32)));

    let scan = server
        .post("/v1/admin/database-encryption/scan")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"scope":scope()}))
        .await;
    assert_eq!(scan.status_code(), 200);
    let scan = scan.json::<Value>();
    assert_eq!(scan["totals"]["stale"], 4);
    assert_eq!(scan["totals"]["by_key_id"]["db-v1"], 4);
    let verify = job(&server, "verify", "verify", Some(0)).await;
    assert_eq!(verify["progress"]["pass"], false);
    assert_eq!(verify["progress"]["stale"], 4);
    assert_eq!(
        verify["progress"]["by_key_id"]["fields"]["files.filename"]["db-v1"],
        2
    );
    for field in ["responses.metadata", "response_items.item"] {
        assert_eq!(verify["progress"]["by_key_id"]["fields"][field]["db-v1"], 1);
    }

    let client = database.pool().get().await.unwrap();
    let before: Vec<(Uuid, String)> = client
        .query("SELECT id,filename FROM files ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    drop(client);
    let dry_run = job(&server, "dry_run", "rewrap", None).await;
    assert_eq!(dry_run["progress"]["stale"], 4);
    assert_eq!(dry_run["progress"]["rewrapped"], 0);
    assert!(dry_run["progress"]["already_current"].as_i64().unwrap() >= 1);
    let client = database.pool().get().await.unwrap();
    let after: Vec<(Uuid, String)> = client
        .query("SELECT id,filename FROM files ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(before, after);
    drop(client);

    let started = std::time::Instant::now();
    let execute = job(&server, "execute", "rewrap", Some(25)).await;
    assert_eq!(execute["scope"]["options"]["batch_delay_ms"], 25);
    assert_eq!(execute["progress"]["rewrapped"], 4);
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
                .get_by_id(item_id, conversation.workspace_id.clone())
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
    let repeat = job(&server, "execute", "rewrap", None).await;
    assert_eq!(repeat["progress"]["rewrapped"], 0);
    let verify = job(&server, "verify", "verify", None).await;
    assert_eq!(verify["progress"]["pass"], true);
    assert_eq!(verify["progress"]["stale"], 0);
    let by_key_id = verify["progress"]["by_key_id"]["totals"]
        .as_object()
        .unwrap();
    assert_eq!(by_key_id.len(), 1);
    assert!(by_key_id.contains_key("db-v2"));

    drop(repository);
    drop(server);
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.database_encryption_keys = Some(vec![KeySourceSpec {
            key_id: "db-v2".into(),
            source: KeySource::Hex("09".repeat(32)),
        }]);
    })
    .await;
    assert_eq!(
        database.pool().encryption_ring().unwrap().read_ids(),
        vec!["db-v2"]
    );
    let repository = database::repositories::file::FileRepository::new(database.pool().clone());
    for id in ids {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().unwrap().filename,
            "rotation.txt"
        );
    }
    assert_eq!(
        job(&server, "verify", "verify", None).await["progress"]["pass"],
        true
    );

    let unknown_id = Uuid::new_v4();
    let unknown = encrypt_with_key_id(
        &[8; 32],
        "db-v3",
        "files",
        "filename",
        unknown_id,
        "unknown.txt",
    )
    .unwrap();
    let plaintext_id = Uuid::new_v4();
    let client = database.pool().get().await.unwrap();
    for (id, filename) in [
        (unknown_id, unknown.clone()),
        (plaintext_id, "plain.txt".into()),
    ] {
        client.execute("INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) VALUES($1,$2,1,'text/plain','assistants','rotation/key',$3,$4)", &[&id,&filename,&workspace,&api_key]).await.unwrap();
    }
    drop(client);
    let invalid = job(&server, "execute", "rewrap", None).await;
    assert_eq!(invalid["status"], "completed");
    assert_eq!(invalid["progress"]["invalid_envelopes"], 1);
    assert_eq!(invalid["progress"]["plaintext"], 1);
    assert_eq!(invalid["progress"]["by_key_id"]["totals"]["db-v3"], 1);
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
    client
        .execute(
            "DELETE FROM files WHERE id=ANY($1)",
            &[&vec![ids[0], ids[1], ids[2], unknown_id, plaintext_id]],
        )
        .await
        .unwrap();
    // Restore ordinary shared-test ring semantics after the reduced-ring check.
    database.pool().set_encryption_ring(
        KeyRing::new(
            "e2e-db-v1".into(),
            database::field_encryption::parse_key(&test_config().database_encryption_key).unwrap(),
            vec![("db-v2".into(), [9; 32])],
        )
        .unwrap(),
    );
    drop(client);
    job(&server, "execute", "rewrap", None).await;
}
