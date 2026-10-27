//! Public receipt verification, ID/billing correlation, and bounded lifecycle.
use crate::common::*;
use axum::{body::Body, http::Request, Router};
use bytes::Bytes;
use inference_providers::{mock::MockProvider, PrivacyClassifyError, ProviderTier};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

struct Fixture {
    server: axum_test::TestServer,
    router: Router,
    database: Arc<database::Database>,
    pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    requests: Arc<Mutex<Vec<Bytes>>>,
    model: String,
    key: String,
    org_id: uuid::Uuid,
}

impl Fixture {
    async fn new(raw: Bytes, tier: ProviderTier, fail: bool) -> Self {
        let (server, router, pool, _, database) = setup_test_server_with_pool_and_router().await;
        let model = format!("privacy-signatures-{}", uuid::Uuid::new_v4());
        let mut batch = api::models::BatchUpdateModelApiRequest::new();
        batch.insert(model.clone(), serde_json::from_value(json!({
            "inputCostPerToken":{"amount":1000,"currency":"USD"},
            "outputCostPerToken":{"amount":0,"currency":"USD"},
            "modelDisplayName":"Privacy receipt fixture","modelDescription":"Receipt test",
            "contextLength":10000,"maxOutputLength":1000,"isActive":true,
            "verifiable":tier != ProviderTier::NonAttested,"attestationSupported":tier != ProviderTier::NonAttested,
            "inputModalities":["text"],"outputModalities":["text"],"providerType":"vllm"
        })).unwrap());
        admin_batch_upsert_models(&server, batch, get_session_id()).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        pool.register_provider(
            model.clone(),
            Arc::new(
                MockProvider::new_accept_all()
                    .with_tier(tier)
                    .with_chat_signature_support(false)
                    .with_privacy_classify_handler(move |body| {
                        seen.lock().unwrap().push(body);
                        if fail {
                            Err(PrivacyClassifyError::HttpError {
                                status_code: 422,
                                message: "fixture rejection".into(),
                            })
                        } else {
                            Ok(raw.clone())
                        }
                    }),
            ),
        )
        .await;
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
        let key = get_api_key_for_org(&server, org.id).await;
        Self {
            server,
            router,
            database,
            pool,
            requests,
            model,
            key,
            org_id,
        }
    }

    fn request(&self, batch: bool) -> String {
        serde_json::to_string_pretty(&json!({"model":self.model,
            "input":if batch {json!(["alice@example.com","bob@example.com"])} else {json!("alice@example.com")},
            "threshold":0.75})).unwrap()
    }

    async fn post(&self, path: &str, body: &str) -> axum_test::TestResponse {
        self.server
            .post(path)
            .add_header("Authorization", format!("Bearer {}", self.key))
            .add_header("Content-Type", "application/json")
            .bytes(Bytes::from(body.to_owned()))
            .await
    }

    async fn verify(&self, id: &str, request: &str, response: &str) {
        let expected = format!("{}:{}", compute_sha256(request), compute_sha256(response));
        for algo in ["ecdsa", "ed25519"] {
            // Immediate, single-attempt public lookup, after the HTTP response.
            let receipt = self
                .server
                .get(&format!("/v1/signature/{id}?signing_algo={algo}"))
                .add_header("Authorization", format!("Bearer {}", self.key))
                .await;
            assert_eq!(receipt.status_code(), 200, "{}", receipt.text());
            let receipt: Value = receipt.json();
            assert_eq!(receipt["signature_kind"], "gateway");
            assert_eq!(receipt["text"], expected);
            let signature = receipt["signature"].as_str().unwrap();
            let address = receipt["signing_address"].as_str().unwrap();
            assert!(if algo == "ecdsa" {
                verify_ecdsa_signature(&expected, signature, address)
            } else {
                verify_ed25519_signature(&expected, signature, address)
            });
        }
        assert!(self.pool.get_provider_by_chat_id(id).await.is_none());
    }

    async fn check_billing(&self, inference_id: uuid::Uuid, id: Option<&str>, tokens: i32) {
        let client = self.database.pool().get().await.unwrap();
        let rows=client.query("SELECT inference_id, provider_request_id, input_tokens, output_tokens, total_cost FROM organization_usage_log WHERE organization_id=$1", &[&self.org_id]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, uuid::Uuid>("inference_id"), inference_id);
        assert_eq!(
            rows[0]
                .get::<_, Option<String>>("provider_request_id")
                .as_deref(),
            id
        );
        assert_eq!(rows[0].get::<_, i32>("input_tokens"), tokens);
        assert_eq!(rows[0].get::<_, i32>("output_tokens"), 0);
        let balance = client
            .query_one(
                "SELECT total_requests, total_tokens, total_spent FROM organization_balance WHERE organization_id=$1",
                &[&self.org_id],
            )
            .await
            .unwrap();
        assert_eq!(balance.get::<_, i64>("total_requests"), 1);
        assert_eq!(balance.get::<_, i64>("total_tokens"), i64::from(tokens));
        assert_eq!(
            balance.get::<_, i64>("total_spent"),
            rows[0].get::<_, i64>("total_cost"),
        );
        assert_eq!(self.requests.lock().unwrap().len(), 1);
    }
}

fn upstream(id: Option<&str>, batch: bool) -> Bytes {
    let mut data = vec![
        json!({"index":0,"spans":[{"category":"private_email","start":0,"end":17,"score":0.99}],"usage":{"input_tokens":7}}),
    ];
    if batch {
        data.push(json!({"index":1,"spans":[{"category":"private_email","start":0,"end":15,"score":0.99}],"usage":{"input_tokens":9}}));
    }
    let mut body =
        json!({"model":"privacy-upstream","data":data,"future_extension":{"preserved":true}});
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    Bytes::from(serde_json::to_string_pretty(&body).unwrap())
}

async fn verify_endpoint(path: &str) {
    for tier in [ProviderTier::Near, ProviderTier::NonAttested] {
        for batch in [false, true] {
            let id = format!("privacy-{}", uuid::Uuid::new_v4());
            let raw = upstream(Some(&id), batch);
            let fixture = Fixture::new(raw.clone(), tier, false).await;
            let request = fixture.request(batch);
            let response = fixture.post(path, &request).await;
            assert_eq!(response.status_code(), 200, "{}", response.text());
            let body: Value = response.json();
            assert_eq!(body["id"], id);
            let inference_id = services::completions::hash_inference_id_to_uuid(&id);
            assert_eq!(
                response.header("Inference-Id"),
                inference_id.to_string().as_str()
            );
            assert!(!response.headers().contains_key("x-generation-id"));
            assert!(!response.headers().contains_key("x-signature-id"));
            let forwarded = fixture.requests.lock().unwrap()[0].clone();
            if path.ends_with("/classify") {
                assert_eq!(response.as_bytes(), &raw);
                assert_eq!(forwarded.as_ref(), request.as_bytes());
            } else {
                assert_ne!(response.as_bytes(), &raw);
                assert!(!response.text().contains("alice@example.com"));
                assert_eq!(body["data"][0]["redacted"], "redacted1@example.com");
                if batch {
                    assert_eq!(body["data"][1]["redacted"], "redacted2@example.com");
                }
                assert_ne!(forwarded.as_ref(), request.as_bytes());
                assert!(serde_json::from_slice::<Value>(&forwarded).unwrap()["input"].is_array());
            }
            fixture.verify(&id, &request, &response.text()).await;
            fixture
                .check_billing(inference_id, Some(&id), if batch { 16 } else { 7 })
                .await;
        }
    }
}

#[tokio::test]
async fn classify_signs_exact_bytes_and_correlates_billing() {
    verify_endpoint("/v1/privacy/classify").await;
}
#[tokio::test]
async fn redact_signs_final_bytes_and_preserves_upstream_id() {
    verify_endpoint("/v1/privacy/redact").await;
}

#[tokio::test]
async fn missing_or_invalid_upstream_ids_preserve_legacy_results_and_usage() {
    for id in [None, Some(""), Some("invalid\nidentifier")] {
        for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
            let raw = upstream(id, false);
            let fixture = Fixture::new(raw.clone(), ProviderTier::Near, false).await;
            let response = fixture.post(path, &fixture.request(false)).await;
            assert_eq!(response.status_code(), 200);
            if path.ends_with("/classify") {
                assert_eq!(response.as_bytes(), &raw);
            } else {
                assert!(response.json::<Value>().get("id").is_none());
            }
            let inference_id =
                uuid::Uuid::parse_str(response.header("inference-id").to_str().unwrap()).unwrap();
            assert_eq!(inference_id.get_version_num(), 4);
            fixture.check_billing(inference_id, None, 7).await;
            let client = fixture.database.pool().get().await.unwrap();
            let count: i64 = client
                .query_one(
                    "SELECT count(*) FROM chat_signatures WHERE chat_id=$1",
                    &[&inference_id.to_string()],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 0);
        }
    }
}

#[tokio::test]
async fn upstream_errors_do_not_create_receipts_or_usage() {
    for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
        let id = format!("privacy-{}", uuid::Uuid::new_v4());
        let fixture = Fixture::new(upstream(Some(&id), false), ProviderTier::Near, true).await;
        let response = fixture.post(path, &fixture.request(false)).await;
        assert_eq!(response.status_code(), 422);
        assert!(!response.headers().contains_key("inference-id"));
        let client = fixture.database.pool().get().await.unwrap();
        let usage: i64 = client
            .query_one(
                "SELECT count(*) FROM organization_usage_log WHERE organization_id=$1",
                &[&fixture.org_id],
            )
            .await
            .unwrap()
            .get(0);
        let receipts: i64 = client
            .query_one(
                "SELECT count(*) FROM chat_signatures WHERE chat_id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!((usage, receipts), (0, 0));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }
}

/// Block only this test's receipt insert, without shared-table locks or DDL.
async fn block_receipt(tx: &tokio_postgres::Transaction<'_>, id: &str) -> i32 {
    tx.execute("INSERT INTO chat_signatures (chat_id,text,signature,signing_address,signing_algo) VALUES ($1,'pending','pending','pending','ecdsa')", &[&id]).await.unwrap();
    tx.query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn signature_timeout_keeps_response_and_single_billing() {
    for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
        let id = format!("privacy-{}", uuid::Uuid::new_v4());
        let fixture = Fixture::new(upstream(Some(&id), false), ProviderTier::Near, false).await;
        let mut blocker = fixture.database.pool().get().await.unwrap();
        let tx = blocker.transaction().await.unwrap();
        block_receipt(&tx, &id).await;
        let start = std::time::Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(9),
            fixture.post(path, &fixture.request(false)),
        )
        .await
        .expect("signature finalization must be bounded");
        assert_eq!(response.status_code(), 200);
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert_eq!(response.json::<Value>()["id"], id);
        fixture
            .check_billing(
                services::completions::hash_inference_id_to_uuid(&id),
                Some(&id),
                7,
            )
            .await;
        assert!(fixture.pool.get_provider_by_chat_id(&id).await.is_none());
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn usage_timeout_returns_success_and_retries_with_the_same_billing_id() {
    for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
        // The fallback UUID must also survive retries; only deriving a stable
        // ID for newer providers would leave the legacy path double-billed.
        for has_id in [true, false] {
            let id = has_id.then(|| format!("privacy-{}", uuid::Uuid::new_v4()));
            let raw = upstream(id.as_deref(), false);
            let fixture = Fixture::new(raw, ProviderTier::Near, false).await;
            let mut blocker = fixture.database.pool().get().await.unwrap();
            let tx = blocker.transaction().await.unwrap();
            tx.query_one(
                "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
                &[&fixture.org_id],
            )
            .await
            .unwrap();
            let blocker_pid: i32 = tx
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            let client = fixture.database.pool().get().await.unwrap();
            let request = fixture.request(false);
            let started = std::time::Instant::now();
            let (response, ()) = tokio::join!(
                async {
                    tokio::time::timeout(Duration::from_secs(12), fixture.post(path, &request))
                        .await
                        .expect("billing must not indefinitely delay a completed inference")
                },
                async {
                    let writer = tokio::time::timeout(Duration::from_secs(4), async {
                        loop {
                            let rows = client
                                .query(
                                    "SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND $1 = ANY(pg_blocking_pids(pid))",
                                    &[&blocker_pid],
                                )
                                .await
                                .unwrap();
                            if let Some(row) = rows.first() {
                                break row.get::<_, i32>(0);
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .unwrap();
                    // Cross the actual five-second deadline, then cancel only
                    // this test's original statement. A later usage row must
                    // come from the retry, not a write surviving future drop.
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    let cancelled: bool = client
                        .query_one(
                            "SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid=$1 AND $2 = ANY(pg_blocking_pids(pid))",
                            &[&writer, &blocker_pid],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    assert!(cancelled);
                }
            );
            // Keep the lock until after the response, so the actual five-second
            // settlement deadline expires. The old implementation returned 500.
            assert_eq!(response.status_code(), 200, "{}", response.text());
            assert!(started.elapsed() >= Duration::from_secs(5));
            let inference_id =
                uuid::Uuid::parse_str(response.header("inference-id").to_str().unwrap()).unwrap();
            if let Some(id) = &id {
                assert_eq!(
                    inference_id,
                    services::completions::hash_inference_id_to_uuid(id),
                );
                fixture.verify(id, &request, &response.text()).await;
            } else {
                assert_eq!(inference_id.get_version_num(), 4);
            }
            let before: i64 = client
                .query_one(
                    "SELECT count(*) FROM organization_usage_log WHERE organization_id=$1",
                    &[&fixture.org_id],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(before, 0);
            tx.rollback().await.unwrap();
            // The original statement was cancelled; the retry must persist the
            // header's exact ID before we check both the usage row and balance.
            tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    let recorded: i64 = client
                        .query_one(
                            "SELECT count(*) FROM organization_usage_log WHERE organization_id=$1",
                            &[&fixture.org_id],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    if recorded > 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            fixture.check_billing(inference_id, id.as_deref(), 7).await;
        }
    }
}

#[tokio::test]
async fn finalization_survives_disconnect_during_usage_settlement() {
    for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
        let id = format!("privacy-{}", uuid::Uuid::new_v4());
        let raw = upstream(Some(&id), false);
        let fixture = Fixture::new(raw, ProviderTier::Near, false).await;
        let mut blocker = fixture.database.pool().get().await.unwrap();
        let tx = blocker.transaction().await.unwrap();
        // Block billing before any signature statement is sent. Blocking only
        // a receipt INSERT would be insufficient: PostgreSQL can finish an
        // already-sent statement even after its Rust future is dropped.
        tx.query_one(
            "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
            &[&fixture.org_id],
        )
        .await
        .unwrap();
        let blocker_pid: i32 = tx
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let request = fixture.request(false);
        let router = fixture.router.clone();
        let http = Request::builder()
            .method("POST")
            .uri(path)
            .header("Authorization", format!("Bearer {}", fixture.key))
            .header("Content-Type", "application/json")
            .body(Body::from(request))
            .unwrap();
        let task = tokio::spawn(async move { router.oneshot(http).await });
        let client = fixture.database.pool().get().await.unwrap();
        tokio::time::timeout(Duration::from_secs(4),async {
            loop {
                let waiting:bool=client.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))", &[&blocker_pid]).await.unwrap().get(0);
                if waiting {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        let before: i64 = client
            .query_one(
                "SELECT count(*) FROM chat_signatures WHERE chat_id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(before, 0);
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tx.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(4),async {
            loop {
                let count:i64=client.query_one("SELECT count(*) FROM chat_signatures WHERE chat_id=$1 AND signature_kind='gateway'", &[&id]).await.unwrap().get(0);
                if count==2 {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        fixture
            .check_billing(
                services::completions::hash_inference_id_to_uuid(&id),
                Some(&id),
                7,
            )
            .await;
        assert!(fixture.pool.get_provider_by_chat_id(&id).await.is_none());
    }
}

#[tokio::test]
async fn signature_storage_error_keeps_response_and_single_billing() {
    for path in ["/v1/privacy/classify", "/v1/privacy/redact"] {
        let id = format!("privacy-{}", uuid::Uuid::new_v4());
        let fixture = Fixture::new(upstream(Some(&id), false), ProviderTier::Near, false).await;
        let mut blocker = fixture.database.pool().get().await.unwrap();
        let tx = blocker.transaction().await.unwrap();
        let blocker_pid = block_receipt(&tx, &id).await;
        let router = fixture.router.clone();
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("Authorization", format!("Bearer {}", fixture.key))
            .header("Content-Type", "application/json")
            .body(Body::from(fixture.request(false)))
            .unwrap();
        let task = tokio::spawn(async move { router.oneshot(request).await });
        let client = fixture.database.pool().get().await.unwrap();
        // Cancel only the receipt statement blocked by this test's transaction.
        // This creates a real repository error without changing shared schema.
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let rows = client.query("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE datname=current_database() AND $1 = ANY(pg_blocking_pids(pid))", &[&blocker_pid]).await.unwrap();
                if !rows.is_empty() {
                    assert_eq!(rows.len(), 1);
                    assert!(rows[0].get::<_, bool>(0));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 200);
        tx.rollback().await.unwrap();
        let receipts: i64 = client
            .query_one(
                "SELECT count(*) FROM chat_signatures WHERE chat_id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(receipts, 0);
        fixture
            .check_billing(
                services::completions::hash_inference_id_to_uuid(&id),
                Some(&id),
                7,
            )
            .await;
        assert!(fixture.pool.get_provider_by_chat_id(&id).await.is_none());
    }
}
