use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use bytes::Bytes;
use inference_providers::{
    mock::MockProvider, CompletionError, InferenceProvider, ProviderTier,
    SystemOneResponseWithBytes,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wiremock::{
    matchers::{body_partial_json, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

fn request(model: &str) -> Value {
    json!({"model":model,"state":{"message":"Please refund this charge"},
        "questions":{"billing":{"type":"noul","instructions":"Is this about billing?"}}})
}

fn result(id: Option<&str>) -> Value {
    let mut value = json!({"model":"jev-1.13.0","answers":{"billing":{"type":"noul","noul":0.95}},
        "usage":{"input_tokens":10,"output_tokens":3},"future_extension":{"preserved":true}});
    if let Some(id) = id {
        value["id"] = json!(id);
    }
    value
}

async fn catalog(
    server: &axum_test::TestServer,
    provider: Option<Value>,
    active: bool,
) -> (String, String) {
    let model = format!("typesafe/jev-{}", uuid::Uuid::new_v4());
    let alias = format!("jev-alias-{}", uuid::Uuid::new_v4());
    catalog_with_names(server, provider, active, model, alias).await
}

async fn catalog_with_names(
    server: &axum_test::TestServer,
    provider: Option<Value>,
    active: bool,
    model: String,
    alias: String,
) -> (String, String) {
    let mut config = json!({
        "inputCostPerToken":{"amount":3,"currency":"USD"},
        "outputCostPerToken":{"amount":2,"currency":"USD"},
        "modelDisplayName":"Jev decision fixture","modelDescription":"System One integration fixture",
        "contextLength":32768,"maxOutputLength":512,"verifiable":true,
        "isActive":active,"attestationSupported":true,
        "inputModalities":["text"],"outputModalities":["decisions"],"aliases":[alias],
        "providerType":"vllm"
    });
    if let Some(provider) = provider {
        config["providerType"] = json!("external");
        config["providerConfig"] = provider;
        config["verifiable"] = json!(false);
        config["attestationSupported"] = json!(false);
    }
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(model.clone(), serde_json::from_value(config).unwrap());
    admin_batch_upsert_models(server, batch, get_session_id()).await;
    (model, alias)
}

async fn auth(server: &axum_test::TestServer) -> String {
    let org = setup_org_with_credits(server, 10_000_000_000).await;
    get_api_key_for_org(server, org.id).await
}

async fn assert_signatures(
    server: &axum_test::TestServer,
    key: &str,
    id: &str,
    kind: &str,
    req: &str,
    res: &str,
) {
    let expected = format!("{}:{}", compute_sha256(req), compute_sha256(res));
    for algorithm in ["ecdsa", "ed25519"] {
        let response = server
            .get(&format!("/v1/signature/{id}?signing_algo={algorithm}"))
            .add_header("Authorization", format!("Bearer {key}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let signature: Value = response.json();
        assert_eq!(signature["signature_kind"], kind);
        assert_eq!(signature["text"], expected);
        let hex = signature["signature"].as_str().unwrap();
        let address = signature["signing_address"].as_str().unwrap();
        assert!(if algorithm == "ecdsa" {
            verify_ecdsa_signature(&expected, hex, address)
        } else {
            verify_ed25519_signature(&expected, hex, address)
        });
    }
}

#[tokio::test]
async fn systemone_external_catalog_alias_billing_and_gateway_signatures() {
    let upstream = MockServer::start().await;
    let raw = serde_json::to_string_pretty(&result(None)).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer fixture-typesafe-key"))
        .and(body_partial_json(
            json!({"model":"jev-1.13.0","state":{"message":"Please refund this charge"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(raw.clone(), "application/json"))
        .expect(2)
        .mount(&upstream)
        .await;
    let (server, _, _, database) = setup_test_server_with_pool().await;
    let (model, alias) = catalog(
        &server,
        Some(json!({
            "backend":"typesafe","base_url":format!("{}/v1",upstream.uri()),
            "model_name":"jev-1.13.0","api_key":"fixture-typesafe-key"
        })),
        true,
    )
    .await;
    let key = auth(&server).await;
    let models: Value = server.get("/v1/models").await.json();
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == model)
        .unwrap();
    assert_eq!(entry["output_modalities"], json!(["decisions"]));
    assert_eq!(entry["input_modalities"], json!(["text"]));
    assert!(!models.to_string().contains("fixture-typesafe-key"));
    let request_text = serde_json::to_string_pretty(&request(&alias)).unwrap();
    let mut previous_id = None;
    for _ in 0..2 {
        let response = server
            .post("/v1/systemone")
            .add_header("Authorization", format!("Bearer {key}"))
            .add_header("Content-Type", "application/json")
            .bytes(Bytes::from(request_text.clone()))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert_eq!(response.text(), raw);
        assert_eq!(response.header("x-serving-provider"), "non-attested");
        assert_eq!(
            response.header("x-model-alias-resolved"),
            format!("{alias} -> {model}").as_str()
        );
        let id = response
            .header("x-signature-id")
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(Some(&id), previous_id.as_ref());
        previous_id = Some(id.clone());
        assert_signatures(&server, &key, &id, "gateway", &request_text, &raw).await;
        let inference_id =
            uuid::Uuid::parse_str(response.header("inference-id").to_str().unwrap()).unwrap();
        assert_eq!(
            inference_id,
            services::completions::hash_inference_id_to_uuid(&id)
        );
        let client = database.pool().get().await.unwrap();
        let row = client.query_one(
            "SELECT input_tokens, output_tokens, total_cost, inference_type, provider_request_id, served_provider_tier
             FROM organization_usage_log WHERE inference_id = $1", &[&inference_id]).await.unwrap();
        assert_eq!(row.get::<_, i32>("input_tokens"), 10);
        assert_eq!(row.get::<_, i32>("output_tokens"), 3);
        assert_eq!(row.get::<_, i64>("total_cost"), 36);
        assert_eq!(row.get::<_, String>("inference_type"), "decisions");
        assert_eq!(row.get::<_, String>("provider_request_id"), id);
        assert_eq!(row.get::<_, String>("served_provider_tier"), "non_attested");
        let costs: Value = server
            .post("/v1/billing/costs")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&json!({"requestIds":[inference_id]}))
            .await
            .json();
        assert_eq!(costs["requests"][0]["costNanoUsd"], 36);
    }
}

#[tokio::test]
async fn systemone_uses_actual_provider_trust_and_signature_capability() {
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let key = auth(&server).await;
    for (tier, signatures, kind) in [
        (ProviderTier::Near, true, "provider_tee"),
        (ProviderTier::Near, false, "gateway"),
        (ProviderTier::Attested3p, false, "gateway"),
        (ProviderTier::NonAttested, true, "gateway"),
    ] {
        // Deliberately declares TEE metadata in every case. Runtime attribution
        // must still follow the actual provider and its signature capability.
        let (model, _) = catalog(&server, None, true).await;
        let upstream_id = format!("decision-{}", uuid::Uuid::new_v4());
        let raw = serde_json::to_vec_pretty(&result(Some(&upstream_id))).unwrap();
        let response_bytes = raw.clone();
        let provider = Arc::new(
            MockProvider::new_accept_all()
                .with_tier(tier)
                .with_chat_signature_support(signatures)
                .with_systemone_handler(move |req| {
                    SystemOneResponseWithBytes::parse(response_bytes.clone(), &req)
                }),
        );
        pool.register_provider(model.clone(), provider.clone())
            .await;
        let request_text = serde_json::to_string(&request(&model)).unwrap();
        let response = server
            .post("/v1/systemone")
            .add_header("Authorization", format!("Bearer {key}"))
            .add_header("Content-Type", "application/json")
            .bytes(Bytes::from(request_text.clone()))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert_eq!(response.as_bytes(), raw.as_slice());
        assert_eq!(
            response.header("x-serving-provider"),
            match tier {
                ProviderTier::Near => "near",
                ProviderTier::Attested3p => "chutes",
                ProviderTier::NonAttested => "non-attested",
            }
        );
        let id = response
            .header("x-signature-id")
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(id == upstream_id, kind == "provider_tee");
        if kind == "provider_tee" {
            // MockProvider emits deterministic placeholder signatures. For TEE
            // receipts, assert verbatim preservation; gateway receipts below
            // use real local keys and are verified cryptographically.
            for algorithm in ["ecdsa", "ed25519"] {
                let expected = provider
                    .get_signature(&id, Some(algorithm.into()))
                    .await
                    .unwrap();
                let stored: Value = server
                    .get(&format!("/v1/signature/{id}?signing_algo={algorithm}"))
                    .add_header("Authorization", format!("Bearer {key}"))
                    .await
                    .json();
                assert_eq!(stored["signature_kind"], "provider_tee");
                assert_eq!(stored["signature"], expected.signature);
                assert_eq!(stored["signing_address"], expected.signing_address);
                assert_eq!(stored["signing_algo"], algorithm);
                assert_eq!(
                    stored["text"],
                    format!(
                        "{}:{}",
                        compute_sha256(&request_text),
                        compute_sha256(&response.text())
                    )
                );
            }
            assert!(provider.unpinned_chat_ids().contains(&id));
        } else {
            assert_signatures(&server, &key, &id, kind, &request_text, &response.text()).await;
        }
    }
}

#[tokio::test]
async fn systemone_rejects_invalid_requests_before_inference() {
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let key = auth(&server).await;
    let (model, alias) = catalog(&server, None, true).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    pool.register_provider(
        model.clone(),
        Arc::new(
            MockProvider::new_accept_all().with_systemone_handler(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                panic!("invalid requests must not reach inference");
            }),
        ),
    )
    .await;
    let mut invalid = vec![
        json!({"model":model,"state":"x","questions":{}}),
        json!({"model":model,"state":null,"questions":{"q":{"type":"noul"}}}),
        json!({"model":model,"state":"x","questions":{"q":{"type":"choice","criteria":{}}}}),
        json!({"model":model,"state":"x","questions":{"q":{"type":"score","criteria":[]}}}),
        request("unknown-model"),
    ];
    let mut streaming = request(&model);
    streaming["stream"] = json!(true);
    invalid.push(streaming);
    let (inactive, _) = catalog(&server, None, false).await;
    invalid.push(request(&inactive));
    setup_qwen_model(&server).await;
    invalid.push(request(E2E_QWEN_MODEL_NAME));
    for body in invalid {
        let response = server
            .post("/v1/systemone")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&body)
            .await;
        assert!(
            matches!(response.status_code().as_u16(), 400 | 422),
            "{}",
            response.text()
        );
    }
    for identifier in [&model, &alias] {
        for stream in [false, true] {
            server.post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&json!({"model":identifier,"messages":[{"role":"user","content":"decision"}],"stream":stream}))
            .await.assert_status_bad_request();
            server
                .post("/v1/completions")
                .add_header("Authorization", format!("Bearer {key}"))
                .json(&json!({"model":identifier,"prompt":"decision","stream":stream}))
                .await
                .assert_status_bad_request();
            let response = server
                .post("/v1/responses")
                .add_header("Authorization", format!("Bearer {key}"))
                .json(&json!({"model":identifier,"input":"decision","stream":stream}))
                .await;
            assert_eq!(response.status_code(), 400, "{}", response.text());
            assert!(response.text().contains("/v1/systemone"));
        }
    }
    server
        .post("/v1/systemone")
        .json(&request(&model))
        .await
        .assert_status_unauthorized();
    server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .add_header("x-no-aliasing", "true")
        .json(&request(&alias))
        .await
        .assert_status_bad_request();
    server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .add_header("x-encryption-version", "2")
        .json(&request(&model))
        .await
        .assert_status_bad_request();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn systemone_enforces_credit_and_concurrency_limits_and_releases_slot() {
    use tower::ServiceExt;
    let upstream = MockServer::start().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(result(None))
                .set_delay(std::time::Duration::from_millis(300))
        })
        .expect(2)
        .mount(&upstream)
        .await;
    let (server, router, _, _, _) = setup_test_server_with_pool_and_router().await;
    let (model, _) = catalog(
        &server,
        Some(json!({
            "backend":"typesafe","base_url":format!("{}/v1",upstream.uri()),
            "api_key":"fixture-key"
        })),
        true,
    )
    .await;
    let empty_org = create_org(&server).await;
    let empty_key = get_api_key_for_org(&server, empty_org.id).await;
    let denied = server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {empty_key}"))
        .json(&request(&model))
        .await;
    assert_eq!(denied.status_code(), 402, "{}", denied.text());
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    server
        .patch(&format!(
            "/v1/admin/organizations/{}/concurrent-limit",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"concurrentLimit":1}))
        .await
        .assert_status_ok();
    let first_request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/systemone")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&request(&model)).unwrap(),
        ))
        .unwrap();
    let first = tokio::spawn(async move { router.oneshot(first_request).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let concurrent = server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&request(&model))
        .await;
    assert_eq!(concurrent.status_code(), 429, "{}", concurrent.text());
    assert!(concurrent.text().contains(&model));
    assert!(concurrent.text().contains("Organization limit: 1"));
    assert_eq!(
        concurrent.json::<Value>()["error"]["type"],
        "rate_limit_exceeded"
    );
    assert_eq!(first.await.unwrap().status(), 200);
    server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&request(&model))
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn systemone_retains_upstream_validation_status_without_retry_or_state_echo() {
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let key = auth(&server).await;
    let (model, _) = catalog(&server, None, true).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    pool.register_provider(
        model.clone(),
        Arc::new(
            MockProvider::new_accept_all().with_systemone_handler(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(CompletionError::HttpError {
                    status_code: 422,
                    message: "private-state".into(),
                    is_external: true,
                })
            }),
        ),
    )
    .await;
    let response = server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&request(&model))
        .await;
    assert_eq!(response.status_code(), 422, "{}", response.text());
    assert!(!response.text().contains("private-state"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn systemone_rejects_missing_tee_id_and_invalid_usage_without_billing() {
    let (server, pool, _, database) = setup_test_server_with_pool().await;
    let key = auth(&server).await;
    for missing_id in [true, false] {
        let (model, _) = catalog(&server, None, true).await;
        let mut value = result(None);
        if !missing_id {
            value["usage"]["input_tokens"] = json!(-10);
        }
        let raw = serde_json::to_vec(&value).unwrap();
        pool.register_provider(
            model.clone(),
            Arc::new(
                MockProvider::new_accept_all()
                    .with_tier(ProviderTier::Near)
                    .with_systemone_handler(move |req| {
                        SystemOneResponseWithBytes::parse(raw.clone(), &req)
                    }),
            ),
        )
        .await;
        let response = server
            .post("/v1/systemone")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&request(&model))
            .await;
        assert_eq!(response.status_code(), 502, "{}", response.text());
        let client = database.pool().get().await.unwrap();
        let count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM organization_usage_log WHERE model_name = $1",
                &[&model],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn systemone_fallback_respects_trust_and_organization_policy() {
    let (_, pool, _, _) = setup_test_server_with_pool().await;
    let model = format!("systemone-fallback-{}", uuid::Uuid::new_v4());
    let primary = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Near)
            .with_systemone_handler(|_| {
                Err(CompletionError::HttpError {
                    status_code: 503,
                    message: "unavailable".into(),
                    is_external: false,
                })
            }),
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let fallback = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Attested3p)
            .with_chat_signature_support(false)
            .with_systemone_handler(move |req| {
                counter.fetch_add(1, Ordering::SeqCst);
                SystemOneResponseWithBytes::parse(serde_json::to_vec(&result(None)).unwrap(), &req)
            }),
    );
    pool.register_provider(model.clone(), primary).await;
    pool.register_pinned_secondary_provider(model.clone(), fallback, None)
        .await;
    // Plaintext fallback must never be tried when an attested provider exists.
    pool.register_pinned_secondary_provider(
        model.clone(),
        Arc::new(
            MockProvider::new_accept_all().with_systemone_handler(|_| panic!("trust downgrade")),
        ),
        None,
    )
    .await;
    let req = serde_json::from_value(request(&model)).unwrap();
    let served = pool
        .systemone_with_attribution(req, "hash".into(), false)
        .await
        .unwrap();
    assert_eq!(
        served.signature_kind,
        services::attestation::SignatureKind::Gateway
    );
    assert!(served.provider_attribution.served_via_fallback);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let req = serde_json::from_value(request(&model)).unwrap();
    assert!(pool
        .systemone_with_attribution(req, "hash".into(), true)
        .await
        .is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn systemone_invalid_success_and_timeout_never_invoke_a_second_provider() {
    let (_, pool, _, _) = setup_test_server_with_pool().await;
    for scenario in ["invalid_payload", "missing_receipt", "timeout"] {
        let model = format!("systemone-no-replay-{}", uuid::Uuid::new_v4());
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let calls = primary_calls.clone();
        pool.register_provider(
            model.clone(),
            Arc::new(
                MockProvider::new_accept_all()
                    .with_tier(ProviderTier::Near)
                    .with_systemone_handler(move |req| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if scenario == "timeout" {
                            Err(CompletionError::Timeout {
                                operation: "systemone".into(),
                                timeout_seconds: 1,
                            })
                        } else {
                            let mut body =
                                result((scenario != "missing_receipt").then_some("tee-id"));
                            if scenario == "invalid_payload" {
                                body["answers"]["billing"]["noul"] = json!(2.0);
                            }
                            SystemOneResponseWithBytes::parse(
                                serde_json::to_vec(&body).unwrap(),
                                &req,
                            )
                        }
                    }),
            ),
        )
        .await;
        pool.register_pinned_secondary_provider(
            model.clone(),
            Arc::new(
                MockProvider::new_accept_all()
                    .with_tier(ProviderTier::Attested3p)
                    .with_systemone_handler(|_| panic!("must not issue a second paid inference")),
            ),
            None,
        )
        .await;
        let error = pool
            .systemone_with_attribution(
                serde_json::from_value(request(&model)).unwrap(),
                "hash".into(),
                false,
            )
            .await
            .err()
            .expect("invalid System One responses must fail without fallback");
        assert!(matches!(
            error,
            CompletionError::InvalidResponse(_) | CompletionError::Timeout { .. }
        ));
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn systemone_is_rejected_by_native_responses_even_when_allowlisted() {
    let model = format!("native-decision-{}", uuid::Uuid::new_v4());
    let alias = format!("native-decision-alias-{}", uuid::Uuid::new_v4());
    let (server, pool, _, _) = setup_test_server_with_pool_and_config(|config| {
        config.native_responses_models = vec![model.clone()];
    })
    .await;
    catalog_with_names(&server, None, true, model.clone(), alias.clone()).await;
    pool.register_provider(
        model.clone(),
        Arc::new(
            MockProvider::new_accept_all()
                .with_responses_handler(|_| panic!("decisions must not reach native Responses")),
        ),
    )
    .await;
    let key = auth(&server).await;
    for identifier in [&model, &alias] {
        for stream in [false, true] {
            let response = server
                .post("/v1/responses")
                .add_header("Authorization", format!("Bearer {key}"))
                .json(&json!({"model":identifier,"store":false,"input":"decision","stream":stream}))
                .await;
            assert_eq!(response.status_code(), 400, "{}", response.text());
            assert!(response.text().contains("/v1/systemone"));
        }
    }
}

#[tokio::test]
async fn systemone_finalization_survives_client_disconnect() {
    use std::time::Duration;
    use tower::ServiceExt;

    let (server, router, pool, _, database) = setup_test_server_with_pool_and_router().await;
    let (model, _) = catalog(&server, None, true).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    server
        .patch(&format!(
            "/v1/admin/organizations/{}/concurrent-limit",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"concurrentLimit": 1}))
        .await
        .assert_status_ok();

    let id = format!("decision-{}", uuid::Uuid::new_v4());
    let response_id = id.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let provider = Arc::new(
        MockProvider::new_accept_all()
            .with_tier(ProviderTier::Near)
            .with_systemone_handler(move |req| {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                let id = if attempt == 0 {
                    response_id.clone()
                } else {
                    format!("{response_id}-{attempt}")
                };
                SystemOneResponseWithBytes::parse(
                    serde_json::to_vec(&result(Some(&id))).unwrap(),
                    &req,
                )
            }),
    );
    pool.register_provider(model.clone(), provider.clone())
        .await;
    let request_text = serde_json::to_string(&request(&model)).unwrap();

    // A real database barrier makes cancellation deterministic: inference has
    // succeeded and receipt persistence is pending, rather than merely delayed
    // in the upstream request. Nextest serializes this table-locking test.
    let mut blocker = database.pool().get().await.unwrap();
    let transaction = blocker.transaction().await.unwrap();
    transaction
        .batch_execute("LOCK TABLE chat_signatures IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/systemone")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(request_text.clone()))
        .unwrap();
    let pending = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            transaction.batch_execute("SELECT pg_stat_clear_snapshot()").await.unwrap();
            let blocked: bool = transaction.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND pg_backend_pid() = ANY(pg_blocking_pids(pid)) AND query ILIKE '%INSERT INTO chat_signatures%')",
                &[],
            ).await.unwrap().get(0);
            if blocked { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("receipt write should reach the storage barrier");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(!provider.unpinned_chat_ids().contains(&id));

    // Cancelling the caller must not release its slot before persistence ends.
    server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::from_str::<Value>(&request_text).unwrap())
        .await
        .assert_status_too_many_requests();
    transaction.rollback().await.unwrap();
    drop(blocker);

    let client = database.pool().get().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let receipts: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM chat_signatures WHERE chat_id = $1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            let usage_count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM organization_usage_log WHERE provider_request_id = $1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            if receipts == 2 && usage_count == 1 && provider.unpinned_chat_ids().contains(&id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached finalization should persist both receipts and unpin");
    let row = client.query_one(
        "SELECT COUNT(*), SUM(input_tokens)::BIGINT, SUM(output_tokens)::BIGINT FROM organization_usage_log WHERE provider_request_id = $1",
        &[&id],
    ).await.unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, i64>(1), 10);
    assert_eq!(row.get::<_, i64>(2), 3);
    for algorithm in ["ecdsa", "ed25519"] {
        let expected = provider
            .get_signature(&id, Some(algorithm.into()))
            .await
            .unwrap();
        let stored: Value = server
            .get(&format!("/v1/signature/{id}?signing_algo={algorithm}"))
            .add_header("Authorization", format!("Bearer {key}"))
            .await
            .json();
        assert_eq!(stored["signature"], expected.signature);
        assert_eq!(stored["text"], expected.text);
        assert_eq!(stored["signature_kind"], "provider_tee");
    }
    server
        .post("/v1/systemone")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::from_str::<Value>(&request_text).unwrap())
        .await
        .assert_status_ok();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
