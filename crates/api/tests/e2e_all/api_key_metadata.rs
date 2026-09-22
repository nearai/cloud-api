use crate::common::*;
use std::time::{Duration, Instant};

#[tokio::test]
async fn metadata_lookup_does_not_read_usage_tables() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id).await.remove(0);
    let created = server
        .post(&format!("/v1/workspaces/{}/api-keys", workspace.id))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "name": "metadata-with-limit",
            "expires_at": null,
            "spendLimit": { "amount": 1_000_000_000_i64, "currency": "USD" }
        }))
        .await;
    assert_eq!(created.status_code(), 201);
    let key = created.json::<api::models::ApiKeyResponse>();

    // A usage SELECT would block behind either of these locks. Keep this in a
    // transaction so the locks are released even if the request/assertion fails.
    let mut client = database.pool().get().await.unwrap();
    let transaction = client.transaction().await.unwrap();
    transaction.batch_execute(
        "SET LOCAL lock_timeout = '5s'; LOCK TABLE organization_usage_log, organization_service_usage_log IN ACCESS EXCLUSIVE MODE"
    ).await.unwrap();
    let started = Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        server
            .get(&format!(
                "/v1/workspaces/{}/api-keys/{}",
                workspace.id, key.id
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
    })
    .await
    .expect("metadata lookup must not wait for usage-table locks");
    transaction.rollback().await.unwrap();
    assert_eq!(response.status_code(), 200);
    let metadata = response.json::<serde_json::Value>();
    assert_eq!(metadata["id"], key.id);
    assert_eq!(metadata["workspace_id"], workspace.id);
    assert_eq!(metadata["created_by_user_id"], MOCK_USER_ID);
    assert_eq!(metadata["is_active"], true);
    assert_eq!(metadata["spend_limit"]["amount"], 1_000_000_000_i64);
    assert_eq!(metadata["spend_limit"]["scale"], 9);
    assert!(metadata.get("usage").is_none());
    assert!(metadata["key"].is_null());
    assert!(metadata.get("key_hash").is_none());
    assert!(!response.text().contains(key.key.as_ref().unwrap()));
    println!(
        "METADATA_LOCK_TEST elapsed_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
}

#[tokio::test]
async fn metadata_lookup_enforces_auth_and_workspace_scope() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id).await.remove(0);
    let key =
        create_api_key_in_workspace(&server, workspace.id.clone(), "scoped-metadata".into()).await;
    let path = format!("/v1/workspaces/{}/api-keys/{}", workspace.id, key.id);
    assert_eq!(server.get(&path).await.status_code(), 401);

    let (other_session, _) = setup_unique_test_session(&database).await;
    assert_eq!(
        server
            .get(&path)
            .add_header("Authorization", format!("Bearer {other_session}"))
            .await
            .status_code(),
        403
    );

    let other_org = create_org(&server).await;
    let other_workspace = list_workspaces(&server, other_org.id).await.remove(0);
    for missing_path in [
        format!("/v1/workspaces/{}/api-keys/{}", other_workspace.id, key.id),
        format!(
            "/v1/workspaces/{}/api-keys/{}",
            workspace.id,
            uuid::Uuid::new_v4()
        ),
        format!(
            "/v1/workspaces/{}/api-keys/{}",
            uuid::Uuid::new_v4(),
            key.id
        ),
    ] {
        assert_eq!(
            server
                .get(&missing_path)
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .await
                .status_code(),
            404
        );
    }
}

#[tokio::test]
async fn metadata_lookup_preserves_inactive_state_and_hides_deleted_keys() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id).await.remove(0);
    let key =
        create_api_key_in_workspace(&server, workspace.id.clone(), "inactive-metadata".into())
            .await;
    let path = format!("/v1/workspaces/{}/api-keys/{}", workspace.id, key.id);
    let client = database.pool().get().await.unwrap();
    let key_id = uuid::Uuid::parse_str(&key.id).unwrap();
    client
        .execute(
            "UPDATE api_keys SET is_active = false WHERE id = $1",
            &[&key_id],
        )
        .await
        .unwrap();
    let response = server
        .get(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.json::<serde_json::Value>()["is_active"], false);
    client
        .execute(
            "UPDATE api_keys SET deleted_at = NOW() WHERE id = $1",
            &[&key_id],
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .get(&path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .status_code(),
        404
    );
}

#[tokio::test]
async fn metadata_lookup_hides_inactive_workspaces_and_organizations() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id.clone()).await.remove(0);
    let key = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "parent-state-metadata".into(),
    )
    .await;
    let path = format!("/v1/workspaces/{}/api-keys/{}", workspace.id, key.id);
    let client = database.pool().get().await.unwrap();
    let workspace_id = uuid::Uuid::parse_str(&workspace.id).unwrap();
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    client
        .execute(
            "UPDATE workspaces SET is_active = false WHERE id = $1",
            &[&workspace_id],
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .get(&path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .status_code(),
        404
    );
    client
        .execute(
            "UPDATE workspaces SET is_active = true WHERE id = $1",
            &[&workspace_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&org_id],
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .get(&path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .status_code(),
        404
    );
}

#[derive(Clone, Default)]
struct JsonLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for JsonLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn list_timings_keep_request_ids_in_production_json() {
    use tracing::instrument::WithSubscriber;

    let (server, _) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id).await.remove(0);
    for (filter, expected_per_request) in [
        ("info", 2),
        ("info,workspace_api_key_timing=debug", 9),
        ("info,workspace_api_key_timing=warn", 0),
    ] {
        let logs = JsonLogs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_env_filter(filter)
            .with_writer(move || writer.clone())
            .finish();
        let request_ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
        let responses =
            futures::future::join_all(request_ids.iter().enumerate().map(|(offset, id)| {
                let server = &server;
                let workspace_id = &workspace.id;
                async move {
                    server
                        .get(&format!(
                            "/v1/workspaces/{workspace_id}/api-keys?limit=100&offset={offset}"
                        ))
                        .add_header("Authorization", format!("Bearer {}", get_session_id()))
                        .add_header("x-request-id", id.to_string())
                        .add_header("x-private-test", "CUSTOMER_LOG_SENTINEL")
                        .await
                }
            }))
            .with_subscriber(subscriber)
            .await;
        for response in responses {
            assert_eq!(response.status_code(), 200);
        }
        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(!captured.contains("CUSTOMER_LOG_SENTINEL"));
        let events: Vec<serde_json::Value> = captured
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["target"] == "workspace_api_key_timing")
            .collect();
        assert_eq!(events.len(), 2 * expected_per_request, "{captured}");
        for (offset, id) in request_ids.iter().enumerate() {
            let own_events: Vec<_> = events
                .iter()
                .filter(|event| event["fields"]["request_id"] == id.to_string())
                .collect();
            assert_eq!(own_events.len(), expected_per_request, "{captured}");
            for event in own_events {
                assert!(event.get("span").is_none());
                assert!(event.get("spans").is_none());
                assert_eq!(event["fields"]["workspace_id"], workspace.id);
                if event["fields"]["event"] == "workspace_api_key_list_phase_finished" {
                    assert_eq!(event["fields"]["limit"], 100);
                    assert_eq!(event["fields"]["offset"], offset);
                }
            }
        }
    }
    assert!(services::common::request_context::current_request_id().is_none());
}

#[tokio::test]
async fn metadata_lookup_logs_safe_error_category_for_unavailable_database() {
    use tracing::instrument::WithSubscriber;

    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id).await.remove(0);
    let key =
        create_api_key_in_workspace(&server, workspace.id.clone(), "metadata-error".into()).await;
    database.pool().current().unwrap().close();
    let logs = JsonLogs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_max_level(tracing::Level::ERROR)
        .with_writer(move || writer.clone())
        .finish();
    let request_id = uuid::Uuid::new_v4();
    let response = async {
        server
            .get(&format!(
                "/v1/workspaces/{}/api-keys/{}",
                workspace.id, key.id
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("x-request-id", request_id.to_string())
            .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(response.status_code(), 500);
    let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let error = captured
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["fields"]["message"] == "Failed to get API key metadata")
        .expect("metadata failure must have a diagnostic category");
    assert_eq!(error["fields"]["error_category"], "internal_error");
    assert_eq!(error["fields"]["request_id"], request_id.to_string());
    assert!(!captured.contains(key.key.as_ref().unwrap()));
}

/// Synthetic local evidence, not a production latency assertion. Run alone in
/// a dedicated test DB; rows remain available for EXPLAIN after the test.
#[tokio::test]
#[ignore = "opt-in usage-history benchmark; see docs/workspace-api-key-latency.md"]
async fn measure_workspace_key_lookup_latency() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let rows: i32 = std::env::var("KEY_LOOKUP_USAGE_ROWS")
        .unwrap_or_else(|_| "100000".into())
        .parse()
        .unwrap();
    assert!((0..=1_000_000).contains(&rows));
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id.clone()).await.remove(0);
    let mut key_ids = Vec::new();
    for index in 0..64 {
        let key = create_api_key_in_workspace(
            &server,
            workspace.id.clone(),
            format!("benchmark-{index}"),
        )
        .await;
        key_ids.push(uuid::Uuid::parse_str(&key.id).unwrap());
    }
    let model_name = setup_qwen_model(&server).await;
    let service = get_or_create_web_search_service(&server).await;
    let client = database.pool().get().await.unwrap();
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let workspace_id = uuid::Uuid::parse_str(&workspace.id).unwrap();
    let model_id: uuid::Uuid = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            r#"
        INSERT INTO organization_usage_log (
            id, organization_id, workspace_id, api_key_id, model_id, model_name,
            input_tokens, output_tokens, total_tokens, input_cost, output_cost,
            total_cost, inference_type, created_at
        ) SELECT gen_random_uuid(), $1, $2, ($3::uuid[])[1 + ((n - 1) % 64)], $4, $5,
            10, 10, 20, 1, 1, 2, 'chat_completion', NOW() - n * INTERVAL '1 minute'
        FROM generate_series(1, $6::integer) AS n
    "#,
            &[
                &org_id,
                &workspace_id,
                &key_ids,
                &model_id,
                &model_name,
                &rows,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            r#"
        INSERT INTO organization_service_usage_log (
            id, organization_id, workspace_id, api_key_id, service_id, quantity,
            total_cost, inference_id, created_at
        ) SELECT gen_random_uuid(), $1, $2, ($3::uuid[])[1 + ((n - 1) % 64)], $4,
            1, 2, NULL, NOW() - n * INTERVAL '1 minute'
        FROM generate_series(1, $5::integer) AS n
    "#,
            &[&org_id, &workspace_id, &key_ids, &service.id, &rows],
        )
        .await
        .unwrap();
    client.batch_execute("ANALYZE api_keys; ANALYZE organization_usage_log; ANALYZE organization_service_usage_log;").await.unwrap();
    drop(client);

    for concurrency in [1_usize, 4] {
        for (kind, path) in [
            (
                "metadata",
                format!("/v1/workspaces/{}/api-keys/{}", workspace.id, key_ids[0]),
            ),
            (
                "list",
                format!(
                    "/v1/workspaces/{}/api-keys?limit=100&offset=0",
                    workspace.id
                ),
            ),
        ] {
            let mut samples = Vec::new();
            let mut failures = 0;
            // Discard one warm-up batch; collect 20 or 80 samples per case.
            for batch in 0..=20 {
                let results = futures::future::join_all((0..concurrency).map(|_| {
                    let path = &path;
                    let server = &server;
                    async move {
                        let started = Instant::now();
                        let response = tokio::time::timeout(Duration::from_secs(20), async {
                            server
                                .get(path)
                                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                                .await
                        })
                        .await;
                        (
                            started.elapsed().as_secs_f64() * 1000.0,
                            response.is_ok_and(|response| response.status_code() == 200),
                        )
                    }
                }))
                .await;
                if batch > 0 {
                    for (elapsed, ok) in results {
                        samples.push(elapsed);
                        failures += usize::from(!ok);
                    }
                }
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "KEY_LOOKUP_BENCH {}",
                serde_json::json!({
                    "workspace_id": workspace.id, "kind": kind, "keys": 64,
                    "rows_per_usage_table": rows, "concurrency": concurrency,
                    "samples": samples.len(), "failures": failures,
                    "p50_ms": samples[(samples.len() as f64 * 0.5).ceil() as usize - 1],
                    "p95_ms": samples[(samples.len() as f64 * 0.95).ceil() as usize - 1],
                    "max_ms": samples.last().unwrap(),
                })
            );
        }
    }
}
