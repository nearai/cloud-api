//! Management E2E: real HTTP handlers, PostgreSQL, migrations and external
//! provider registration. Transport fault fixtures are explicitly mocked; they
//! are not live vendor or TEE acceptance evidence.
use crate::common::{
    admin_batch_upsert_models, get_session_id, setup_test_server_with_pool, MOCK_USER_AGENT,
};
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

async fn call(
    server: &axum_test::TestServer,
    method: Method,
    path: &str,
    body: Value,
) -> axum_test::TestResponse {
    server
        .method(method, path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn catalog(server: &axum_test::TestServer, active: bool) -> String {
    let name = format!("registry-{}", Uuid::new_v4());
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(name.clone(),serde_json::from_value(json!({
        "modelDisplayName":"Registry model", "modelDescription":"Registry E2E", "aliases":[format!("{name}-alias")], "contextLength":8192,"maxOutputLength":1024,
        "providerType":"external","verifiable":false,"attestationSupported":false,"isActive":active,
        "inputCostPerToken":{"amount":1000000,"currency":"USD"},"outputCostPerToken":{"amount":2000000,"currency":"USD"}
    })).unwrap());
    admin_batch_upsert_models(server, batch, get_session_id()).await;
    name
}

fn definition() -> Value {
    json!({"name":format!("provider-{}",Uuid::new_v4()),"config":{"backend":"openai_compatible","base_url":"https://provider.example.invalid/v1"},"credential_env":null,"models_path":null,"archived":false})
}

async fn create(server: &axum_test::TestServer, body: Value) -> Uuid {
    let response = call(server, Method::POST, "/v1/admin/providers", body).await;
    assert_eq!(
        response.status_code(),
        StatusCode::CREATED,
        "{}",
        response.text()
    );
    let provider = response.json::<Value>();
    assert_eq!(provider["execution_security"], "non_tee");
    Uuid::parse_str(provider["id"].as_str().unwrap()).unwrap()
}

async fn approve(
    server: &axum_test::TestServer,
    id: Uuid,
    name: &str,
    approved: bool,
    archived: bool,
) {
    let response = call(server,Method::PUT,&format!("/v1/admin/providers/{id}/models"),json!({"upstream_model":"remote-model","model_name":name,"approved":approved,"archived":archived,"reasoning_efforts":["low","high"]})).await;
    assert_eq!(
        response.status_code(),
        StatusCode::NO_CONTENT,
        "{}",
        response.text()
    );
}

#[tokio::test]
async fn provider_registry_approval_multiple_providers_and_archive() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let name = catalog(&server, true).await;
    let first_definition = definition();
    let first = create(&server, first_definition.clone()).await;
    let second = create(&server, definition()).await;
    assert!(!pool.has_provider(&name).await);
    approve(&server, first, &name, true, false).await;
    approve(&server, second, &name, true, false).await;
    assert!(pool.has_provider(&name).await);
    let public = server.get("/v1/models").await;
    assert_eq!(public.status_code(), 200);
    let public = public.json::<Value>();
    let model = public["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == name)
        .unwrap();
    let routing = &model["routing"];
    assert_eq!(routing["default_tee_requirement"], "any");
    assert_eq!(routing["profiles"].as_array().unwrap().len(), 2);
    for profile in routing["profiles"].as_array().unwrap() {
        assert_eq!(profile["tee_supported"], false);
        assert_eq!(profile["reasoning_efforts"], json!(["low", "high"]));
        assert!(profile["route_id"].is_string());
        assert!([json!(first), json!(second)].contains(&profile["deployment_id"]));
    }
    let alias = server.get(&format!("/v1/model/{name}-alias")).await;
    assert_eq!(alias.status_code(), 200, "{}", alias.text());
    assert_eq!(alias.json::<Value>()["metadata"]["routing"], *routing);
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let routes = repo.routes().await.unwrap();
    assert_eq!(routes.iter().filter(|(model, _)| model == &name).count(), 2);
    for (_, config) in routes.iter().filter(|(model, _)| model == &name) {
        assert_eq!(config["model_name"], "remote-model");
        assert_eq!(config["reasoning_efforts"], json!(["low", "high"]));
    }
    approve(&server, first, &name, true, true).await;
    assert!(pool.has_provider(&name).await);
    approve(&server, second, &name, false, false).await;
    assert!(!pool.has_provider(&name).await);
    approve(&server, first, &name, true, false).await;
    let mut archived = first_definition.clone();
    archived["archived"] = json!(true);
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/providers/{first}"),
        archived,
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(!pool.has_provider(&name).await);
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/providers/{first}"),
        first_definition,
    )
    .await;
    assert_eq!(response.status_code(), 200);
    assert!(pool.has_provider(&name).await);
    assert!(!repo.models(second).await.unwrap()[0].approved);
    let history: i64 = db
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM provider_registry_history WHERE provider_id=$1",
            &[&first],
        )
        .await
        .unwrap()
        .get(0);
    assert!(history >= 5);
}

#[tokio::test]
async fn provider_registry_model_archive_preserves_identity_and_blocks_reactivation() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    for active in [true, false] {
        let name = catalog(&server, active).await;
        let provider = create(&server, definition()).await;
        approve(&server, provider, &name, true, false).await;
        let repo = database::repositories::ModelRepository::new(db.pool().clone());
        let before = repo.get_by_internal_name(&name).await.unwrap().unwrap();
        for _ in 0..2 {
            let response = call(
                &server,
                Method::PUT,
                &format!("/v1/admin/models/{name}/archive"),
                json!({"archived":true,"change_reason":"E2E archive"}),
            )
            .await;
            assert_eq!(response.status_code(), 204, "{}", response.text());
        }
        assert!(!pool.has_provider(&name).await);
        assert!(services::models::ModelsRepository::resolve_and_get_model(
            &repo,
            &format!("{name}-alias")
        )
        .await
        .unwrap()
        .is_none());
        let response = call(
            &server,
            Method::PATCH,
            "/v1/admin/models",
            json!({&name:{"isActive":true}}),
        )
        .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(
            !repo
                .get_by_internal_name(&name)
                .await
                .unwrap()
                .unwrap()
                .is_active
        );
        let list = call(
            &server,
            Method::GET,
            "/v1/admin/models?include_inactive=true&limit=1000",
            json!({}),
        )
        .await;
        assert_eq!(list.status_code(), 200, "{}", list.text());
        let listed = list.json::<Value>();
        assert!(listed["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["modelId"] == name && m["archived"] == true));
        let history_count = || async {
            db.pool()
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT count(*) FROM model_history WHERE model_id=$1",
                    &[&before.id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0)
        };
        let history_before = history_count().await;
        // Warm the public cache while this model is archived.
        let listed = server
            .get("/v1/model/list?limit=100000")
            .await
            .json::<Value>();
        assert!(!listed["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["modelId"] == name));
        let response = call(
            &server,
            Method::PUT,
            &format!("/v1/admin/models/{name}/archive"),
            json!({"archived":false}),
        )
        .await;
        assert_eq!(response.status_code(), 204, "{}", response.text());
        assert_eq!(history_count().await, history_before + 1);
        let after = repo.get_by_internal_name(&name).await.unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.input_cost_per_token, before.input_cost_per_token);
        assert_eq!(after.is_active, active);
        assert_eq!(pool.has_provider(&name).await, active);
        let listed = server
            .get("/v1/model/list?limit=100000")
            .await
            .json::<Value>();
        assert_eq!(
            listed["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["modelId"] == name),
            active
        );
    }
}

#[tokio::test]
async fn provider_registry_remapping_never_resurrects_legacy_configuration() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let old = catalog(&server, true).await;
    let new = catalog(&server, true).await;
    let response=call(&server,Method::PATCH,"/v1/admin/models",json!({&old:{"providerConfig":{"backend":"openai_compatible","base_url":"https://legacy.example.invalid/v1","api_key":"fixture-only"}}})).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(pool.has_provider(&old).await);
    let id = create(&server, definition()).await;
    approve(&server, id, &old, true, false).await;
    approve(&server, id, &new, true, false).await;
    assert!(!pool.has_provider(&old).await);
    assert!(pool.has_provider(&new).await);
    let fresh = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    assert!(!fresh
        .routes()
        .await
        .unwrap()
        .iter()
        .any(|(name, _)| name == &old));
}

#[tokio::test]
async fn provider_registry_rejects_secrets_invalid_config_and_unknown_models() {
    let (server, _, _, _) = setup_test_server_with_pool().await;
    let mut secret = definition();
    secret["config"]["api_key"] = json!("must-not-store");
    assert_eq!(
        call(&server, Method::POST, "/v1/admin/providers", secret)
            .await
            .status_code(),
        400
    );
    let mut invalid = definition();
    invalid["models_path"] = json!("https://other.example/models");
    assert_eq!(
        call(&server, Method::POST, "/v1/admin/providers", invalid)
            .await
            .status_code(),
        400
    );
    let id = create(&server, definition()).await;
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/providers/{id}/models"),
        json!({"upstream_model":"x","model_name":"does-not-exist","approved":true}),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    let response = call(
        &server,
        Method::PUT,
        "/v1/admin/models/does-not-exist/archive",
        json!({"archived":false}),
    )
    .await;
    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn provider_registry_malformed_pagination_retains_approved_routes() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"remote-model"}]})),
        )
        .mount(&upstream)
        .await;
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let mut body = definition();
    body["config"]["base_url"] = json!(upstream.uri());
    body["models_path"] = json!("models");
    let id = create(&server, body).await;
    let name = catalog(&server, true).await;
    approve(&server, id, &name, true, false).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let before = serde_json::to_value(repo.models(id).await.unwrap()).unwrap();
    upstream.reset().await;
    Mock::given(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[],"has_more":"true"})),
        )
        .mount(&upstream)
        .await;
    let response = call(
        &server,
        Method::POST,
        &format!("/v1/admin/providers/{id}/sync"),
        json!({}),
    )
    .await;
    assert_eq!(response.status_code(), 502, "{}", response.text());
    assert_eq!(
        serde_json::to_value(repo.models(id).await.unwrap()).unwrap(),
        before
    );
    pool.refresh_external_providers(&database::repositories::ModelRepository::new(
        db.pool().clone(),
    ))
    .await
    .unwrap();
    assert!(pool.has_provider(&name).await);
}

/// Credentialed contract gate. Intentionally ignored by default: lack of a live
/// endpoint is not evidence that discovery or inference has passed.
#[tokio::test]
#[ignore = "requires LIVE_PROVIDER_BASE_URL, LIVE_PROVIDER_BACKEND and LIVE_PROVIDER_API_KEY"]
async fn provider_registry_live_discovery_requires_explicit_approval() {
    let (server, _, _, _) = setup_test_server_with_pool().await;
    let mut body = definition();
    body["config"]["base_url"] = std::env::var("LIVE_PROVIDER_BASE_URL")
        .expect("live provider URL required")
        .into();
    body["config"]["backend"] = std::env::var("LIVE_PROVIDER_BACKEND")
        .expect("live adapter required")
        .into();
    body["credential_env"] = json!("LIVE_PROVIDER_API_KEY");
    body["models_path"] = json!("models");
    let id = create(&server, body).await;
    let response = call(
        &server,
        Method::GET,
        &format!("/v1/admin/providers/{id}/models"),
        json!({}),
    )
    .await;
    assert_eq!(response.status_code(), 200);
    let inventory = response.json::<Vec<Value>>();
    assert!(!inventory.is_empty(), "Live discovery must return models");
    assert!(inventory
        .iter()
        .all(|model| model["approved"] == false && model["model_id"].is_null()));
}

/// Real, unauthenticated OpenRouter Models API; no model execution or spend.
#[tokio::test]
#[ignore = "requires access to the public OpenRouter Models API"]
async fn provider_registry_public_openrouter_discovery_preserves_approval() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let mut body = definition();
    body["config"]["base_url"] = json!("https://openrouter.ai/api/v1");
    body["models_path"] = json!("models");
    let id = create(&server, body.clone()).await;
    let path = format!("/v1/admin/providers/{id}/models");
    let response = call(&server, Method::GET, &path, json!({})).await;
    assert_eq!(response.status_code(), 200);
    let inventory = response.json::<Vec<Value>>();
    assert!(
        !inventory.is_empty(),
        "Public discovery failed or unexpectedly returned no models"
    );
    assert!(inventory.iter().all(|row| row["approved"] == false));
    let upstream = inventory[0]["upstream_model"].as_str().unwrap();
    let name = catalog(&server, true).await;
    let approval = json!({"upstream_model":upstream,"model_name":name,"approved":true,"archived":true,"reasoning_efforts":["high"]});
    assert_eq!(
        call(&server, Method::PUT, &path, approval)
            .await
            .status_code(),
        204
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            &format!("/v1/admin/providers/{id}/sync"),
            json!({})
        )
        .await
        .status_code(),
        204
    );
    let refreshed = call(&server, Method::GET, &path, json!({}))
        .await
        .json::<Vec<Value>>();
    let model = refreshed
        .iter()
        .find(|row| row["upstream_model"] == upstream)
        .unwrap();
    assert_eq!(model["approved"], true);
    assert_eq!(model["archived"], true);
    assert_eq!(model["reasoning_efforts"], json!(["high"]));
    assert!(!pool.has_provider(&name).await);
    // A genuine endpoint error must retain the previous complete inventory.
    body["models_path"] = json!(format!("nonexistent-model-list-{}", Uuid::new_v4()));
    let updated = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/providers/{id}"),
        body,
    )
    .await;
    assert_eq!(updated.status_code(), 200, "{}", updated.text());
    assert!(updated.json::<Value>()["sync_error"].is_string());
    let retained = call(&server, Method::GET, &path, json!({}))
        .await
        .json::<Vec<Value>>();
    assert_eq!(retained.len(), refreshed.len());
    let stored = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    )
    .get(id)
    .await
    .unwrap()
    .unwrap();
    assert!(stored.last_synced_at.is_some());
}

#[tokio::test]
async fn provider_registry_read_only_admin_cannot_mutate() {
    let (server, _) = crate::common::setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true
    })
    .await;
    let created=call(&server,Method::POST,"/v1/admin/access-tokens",json!({"name":format!("registry-reader-{}",Uuid::new_v4()),"reason":"Registry E2E","expires_in_hours":1,"permission":"read_only"})).await;
    assert_eq!(created.status_code(), 200, "{}", created.text());
    let token = created.json::<Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = create(&server, definition()).await;
    for (method, path, expected) in [
        (Method::GET, "/v1/admin/providers".to_owned(), 200),
        (Method::GET, format!("/v1/admin/providers/{id}/models"), 200),
        (Method::POST, "/v1/admin/providers".to_owned(), 403),
        (Method::PUT, format!("/v1/admin/providers/{id}"), 403),
        (Method::PUT, format!("/v1/admin/providers/{id}/models"), 403),
        (Method::POST, format!("/v1/admin/providers/{id}/sync"), 403),
        (Method::PUT, "/v1/admin/models/any/archive".to_owned(), 403),
    ] {
        let response = server
            .method(method, &path)
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({}))
            .await;
        assert_eq!(response.status_code(), expected, "{}", response.text());
    }
}

// Transport-fault fixture: validates real adapter dispatch, SQL attempt records,
// and policy enforcement. This is not evidence of a vendor's model semantics.
#[tokio::test]
async fn provider_registry_cost_bounded_failover_records_attempts() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let first_upstream = MockServer::start().await;
    let second_upstream = MockServer::start().await;
    Mock::given(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .expect(1)
        .mount(&first_upstream)
        .await;
    Mock::given(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"transport-fixture", "object":"chat.completion", "created":1,"model":"remote-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}
        })))
        .expect(1).mount(&second_upstream).await;
    let (server, pool, _, db) = crate::common::setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 10;
    })
    .await;
    let name = catalog(&server, true).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let mut provider_ids = Vec::new();
    for upstream in [&first_upstream, &second_upstream] {
        let mut body = definition();
        body["config"]["base_url"] = json!(upstream.uri());
        let id = create(&server, body).await;
        approve(&server, id, &name, true, false).await;
        provider_ids.push(id);
    }
    for (index, id) in provider_ids.iter().enumerate() {
        let inventory_id = repo.models(*id).await.unwrap()[0].id.unwrap();
        let route = repo
            .model_routes(&name)
            .await
            .unwrap()
            .into_iter()
            .find(|route| route.inventory_id == inventory_id)
            .unwrap();
        let rates = json!({"currency":"USD","billing_unit":"million_tokens","input_microunits":1_000_000*(index+1),"output_microunits":1_000_000*(index+1),"cache_read_microunits":null,"cache_write_microunits":null,"provenance":"transport fixture"});
        let response = call(
            &server,
            Method::POST,
            &format!("/v1/admin/provider-routes/{}/cost-profiles", route.id),
            json!({"rates":rates,"change_reason":"Fixture rates"}),
        )
        .await;
        assert_eq!(response.status_code(), 201, "{}", response.text());
    }
    let policy = json!({"default_tee_requirement":"any","mixed_routing_enabled":false,"max_attempts":2,"deadline_ms":5000,"max_attempt_cost_microusd":1000,"max_total_cost_microusd":2000,"allow_negative_margin":true});
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/models/{name}/routing-policy"),
        json!({"policy":policy,"change_reason":"Transport failover fixture"}),
    )
    .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    let params=serde_json::from_value(json!({"model":name,"messages":[{"role":"user","content":"fixture"}],"max_tokens":10,"reasoning_effort":"high"})).unwrap();
    let request_id = Uuid::new_v4();
    let served = pool
        .chat_completion_with_attribution_and_hints(
            params,
            "fixture".into(),
            services::inference_provider_pool::ChatRoutingHints {
                request_id: Some(request_id),
                estimated_tokens: Some(4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        served.provider_attribution.served_deployment_id,
        Some(provider_ids[1])
    );
    assert!(served.provider_attribution.served_route_id.is_some());
    let rows=db.pool().get().await.unwrap().query("SELECT outcome,cost_status,usage_cost_microusd,actual_cost_microusd FROM inference_attempts WHERE accepted_request_id=$1 ORDER BY attempt_index", &[&request_id]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, String>("outcome"), "http_429");
    assert_eq!(rows[0].get::<_, String>("cost_status"), "unknown");
    assert_eq!(rows[1].get::<_, String>("outcome"), "success");
    assert_eq!(rows[1].get::<_, String>("cost_status"), "estimated");
    assert_eq!(
        rows[1].get::<_, Option<i64>>("usage_cost_microusd"),
        Some(12)
    );
    assert_eq!(rows[1].get::<_, Option<i64>>("actual_cost_microusd"), None);
    let health: i64=db.pool().get().await.unwrap().query_one("SELECT count(*) FROM provider_quota_health WHERE quota_group=$1 AND cooldown_until>now()", &[&provider_ids[0].to_string()]).await.unwrap().get(0);
    assert_eq!(health, 1);
    let attempts = call(
        &server,
        Method::GET,
        &format!("/v1/admin/provider-attempts/{request_id}"),
        json!({}),
    )
    .await;
    assert_eq!(attempts.status_code(), 200);
    let attempts = attempts.json::<Value>();
    let attempt_id = attempts[0]["id"].as_str().unwrap();
    let settlement_path = format!("/v1/admin/provider-attempts/{attempt_id}/settlement");
    let settlement = json!({"actual_cost_microusd":7,"evidence":"Invoice fixture 1"});
    for _ in 0..2 {
        let response = call(&server, Method::POST, &settlement_path, settlement.clone()).await;
        assert_eq!(response.status_code(), 204, "{}", response.text());
    }
    let response = call(
        &server,
        Method::POST,
        &settlement_path,
        json!({"actual_cost_microusd":0,"evidence":"Conflicting fixture"}),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    let rows = repo.attempts(request_id).await.unwrap();
    assert_eq!(rows[0]["cost_status"], "settled");
    assert_eq!(rows[0]["actual_cost_microusd"], 7);
    assert_eq!(rows[0]["outcome"], "http_429");
    let mut bounded = policy;
    bounded["max_attempt_cost_microusd"] = json!(0);
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/models/{name}/routing-policy"),
        json!({"policy":bounded,"change_reason":"Enforce zero procurement ceiling"}),
    )
    .await;
    assert_eq!(response.status_code(), 204);
    let blocked_id = Uuid::new_v4();
    let params = serde_json::from_value(
        json!({"model":name,"messages":[{"role":"user","content":"fixture"}],"max_tokens":10}),
    )
    .unwrap();
    assert!(pool
        .chat_completion_with_attribution_and_hints(
            params,
            "fixture".into(),
            services::inference_provider_pool::ChatRoutingHints {
                request_id: Some(blocked_id),
                estimated_tokens: Some(4),
                ..Default::default()
            }
        )
        .await
        .is_err());
    assert!(repo.attempts(blocked_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn provider_registry_accounts_and_cost_versions_preserve_identity() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let name = catalog(&server, true).await;
    let id = create(&server, definition()).await;
    approve(&server, id, &name, true, false).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let route = repo.model_routes(&name).await.unwrap().remove(0);
    let account = repo
        .accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|account| account.id == id)
        .unwrap();
    let mut archived = serde_json::to_value(&account).unwrap();
    archived["archived"] = json!(true);
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/v1/admin/provider-accounts",
            json!({"account":archived,"change_reason":"Archive account"})
        )
        .await
        .status_code(),
        204
    );
    assert!(!pool.has_provider(&name).await);
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/v1/admin/provider-accounts",
            json!({"account":account,"change_reason":"Restore account"})
        )
        .await
        .status_code(),
        204
    );
    assert!(pool.has_provider(&name).await);
    for rate in [1_000_000, 2_000_000] {
        let response=call(&server,Method::POST,&format!("/v1/admin/provider-routes/{}/cost-profiles",route.id),json!({"rates":{"currency":"USD","billing_unit":"million_tokens","input_microunits":rate,"output_microunits":rate,"provenance":"contract"},"change_reason":"Update contract"})).await;
        assert_eq!(response.status_code(), 201, "{}", response.text());
    }
    let profiles = repo.cost_profiles(route.id).await.unwrap();
    assert_eq!(profiles.len(), 2);
    assert_eq!(
        profiles[0]["effective_until"],
        profiles[1]["effective_from"]
    );
    assert!(profiles[1]["effective_until"].is_null());
    assert_eq!(repo.model_routes(&name).await.unwrap()[0].id, route.id);
}

#[tokio::test]
async fn provider_registry_manual_inventory_never_approves_routes() {
    let (server, pool, _, db) = setup_test_server_with_pool().await;
    let name = catalog(&server, true).await;
    let id = create(&server, definition()).await;
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/providers/{id}/inventory"),
        json!({
            "upstream_model":"manual-model","available":true,"archived":false,
            "reasoning_efforts":["none","high"],"provenance":"Operator verified vendor contract",
            "change_reason":"Import unsupported discovery inventory"
        }),
    )
    .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let inventory = repo.models(id).await.unwrap();
    assert_eq!(inventory.len(), 1);
    assert!(!inventory[0].approved);
    assert!(inventory[0].model_id.is_none());
    assert!(repo.model_routes(&name).await.unwrap().is_empty());
    assert!(!pool.registered_model_names().await.contains(&name));
    let row = db
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT provenance,manual_provenance FROM provider_models WHERE provider_id=$1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>("provenance"), "manual");
    assert_eq!(
        row.get::<_, String>("manual_provenance"),
        "Operator verified vendor contract"
    );
}

#[tokio::test]
async fn provider_registry_legacy_remap_preserves_independent_routes() {
    let (server, _, _, db) = setup_test_server_with_pool().await;
    let first = catalog(&server, true).await;
    let second = catalog(&server, true).await;
    let third = catalog(&server, true).await;
    let id = create(&server, definition()).await;
    approve(&server, id, &first, true, false).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let mut route = repo.model_routes(&first).await.unwrap().remove(0);
    let first_route_id = route.id;
    route.id = Uuid::new_v4();
    route.model_id = db
        .pool()
        .get()
        .await
        .unwrap()
        .query_one("SELECT id FROM models WHERE model_name=$1", &[&second])
        .await
        .unwrap()
        .get(0);
    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/models/{second}/routes"),
        json!({"route":route,"change_reason":"Independent alias route"}),
    )
    .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    approve(&server, id, &third, true, false).await;
    assert!(repo.model_routes(&second).await.unwrap()[0].approved);
    let old = repo.model_routes(&first).await.unwrap().remove(0);
    assert_eq!(old.id, first_route_id);
    assert!(!old.approved);
    assert!(repo.model_routes(&third).await.unwrap()[0].approved);
}

#[tokio::test]
async fn provider_registry_shared_circuit_admits_one_probe_and_preserves_new_failure() {
    use services::provider_registry::entities::AttemptRepository;
    let (_, _, _, db) = setup_test_server_with_pool().await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let group = format!("probe-{}", Uuid::new_v4());
    let old_start = chrono::Utc::now() - chrono::Duration::seconds(10);
    repo.record_health(&group, old_start, Some(30_000))
        .await
        .unwrap();
    assert!(!repo.claim_quota(&group).await.unwrap());
    db.pool().get().await.unwrap().execute("UPDATE provider_quota_health SET cooldown_until=now()-interval '1 second' WHERE quota_group=$1", &[&group]).await.unwrap();
    let (a, b, c) = tokio::join!(
        repo.claim_quota(&group),
        repo.claim_quota(&group),
        repo.claim_quota(&group)
    );
    assert_eq!(
        [a.unwrap(), b.unwrap(), c.unwrap()]
            .into_iter()
            .filter(|claimed| *claimed)
            .count(),
        1
    );
    repo.record_health(&group, chrono::Utc::now(), Some(30_000))
        .await
        .unwrap();
    repo.record_health(&group, old_start, None).await.unwrap();
    assert!(!repo.claim_quota(&group).await.unwrap());
}

#[tokio::test]
async fn provider_registry_external_import_shadow_cutover_and_rollback() {
    let (server, _, _, db) = setup_test_server_with_pool().await;
    let name = catalog(&server, true).await;
    let reference = format!(
        "REGISTRY_{}_API_KEY",
        Uuid::new_v4().simple().to_string().to_uppercase()
    );
    std::env::set_var(&reference, "legacy-import-fixture-key");
    let original = json!({"backend":"openai_compatible","base_url":"https://legacy.example.invalid/v1","api_key":"legacy-import-fixture-key","model_name":"legacy-upstream"});
    let client = db.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE models SET provider_config=$2 WHERE model_name=$1",
            &[&name, &original],
        )
        .await
        .unwrap();
    let id: Uuid = client
        .query_one("SELECT id FROM models WHERE model_name=$1", &[&name])
        .await
        .unwrap()
        .get(0);
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    for phase in [
        "plan", "shadow", "shadow", "cutover", "cutover", "rollback", "rollback",
    ] {
        let response = call(&server, Method::POST, "/v1/admin/providers/import-legacy", json!({
            "model_name":name,"credential_env":reference,"phase":phase,"change_reason":"Migration fixture"
        })).await;
        assert_eq!(response.status_code(), 200, "{phase}: {}", response.text());
        assert!(!response.text().contains("legacy-import-fixture-key"));
        assert_eq!(response.json::<Value>()["model_id"], json!(id));
        let routes = repo.routes().await.unwrap();
        let matching: Vec<_> = routes.iter().filter(|(model, _)| model == &name).collect();
        assert_eq!(matching.len(), 1, "{phase}");
        assert_eq!(
            matching[0].1.get("_registry_route").is_some(),
            phase == "cutover",
            "{phase}"
        );
        assert_eq!(matching[0].1["model_name"], "legacy-upstream");
        assert_eq!(matching[0].1["api_key"], "legacy-import-fixture-key");
    }
    let row = client
        .query_one(
            "SELECT id,provider_config,provider_registry_managed FROM models WHERE model_name=$1",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Uuid>("id"), id);
    assert!(!row.get::<_, bool>("provider_registry_managed"));
    let config: Value = row.get("provider_config");
    assert!(config.get("api_key").is_none());
    assert_eq!(config["credential_env"], reference);
    let audits: i64 = client.query_one("SELECT count(*) FROM registry_entity_history WHERE entity_kind='import' AND entity_id=$1", &[&id]).await.unwrap().get(0);
    assert_eq!(audits, 3);
    // A shadowed deployment can be edited, but cutover must revalidate it.
    client.execute("UPDATE inference_providers SET config=jsonb_set(config, '{base_url}', '\"https://changed.example.invalid/v1\"'::jsonb) WHERE id=$1", &[&id]).await.unwrap();
    let plan = call(&server, Method::POST, "/v1/admin/providers/import-legacy", json!({
        "model_name":name,"credential_env":reference,"phase":"plan","change_reason":"Review drift"
    })).await;
    assert_eq!(plan.status_code(), 200);
    assert_eq!(plan.json::<Value>()["shadow_configuration_matches"], false);
    let cutover = call(&server, Method::POST, "/v1/admin/providers/import-legacy", json!({
        "model_name":name,"credential_env":reference,"phase":"cutover","change_reason":"Reject drift"
    })).await;
    assert_eq!(cutover.status_code(), 400, "{}", cutover.text());
    assert!(!client
        .query_one(
            "SELECT provider_registry_managed FROM models WHERE id=$1",
            &[&id]
        )
        .await
        .unwrap()
        .get::<_, bool>(0));
}

#[tokio::test]
async fn provider_registry_organization_requirement_cannot_be_weakened() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(path("/chat/completions")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"org-policy-fixture","object":"chat.completion","created":1,"model":"remote-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))).expect(1).mount(&upstream).await;
    let (server, _, _, db) = crate::common::setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 10
    })
    .await;
    let name = catalog(&server, true).await;
    let mut body = definition();
    body["config"]["base_url"] = json!(upstream.uri());
    let provider = create(&server, body).await;
    approve(&server, provider, &name, true, false).await;
    let org = crate::common::setup_org_with_credits(&server, 10_000_000_000).await;
    let key = crate::common::get_api_key_for_org(&server, org.id.clone()).await;
    let request = json!({"model":name,"messages":[{"role":"user","content":"fixture"}],"tee":"any","max_tokens":4});
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let usage = db.pool().get().await.unwrap().query_one("SELECT served_route_id,served_deployment_id FROM organization_usage_log WHERE model_name=$1 ORDER BY created_at DESC LIMIT 1", &[&name]).await.unwrap();
    assert_eq!(
        usage.get::<_, Option<Uuid>>("served_deployment_id"),
        Some(provider)
    );
    assert!(usage.get::<_, Option<Uuid>>("served_route_id").is_some());
    assert_eq!(
        response.header("x-serving-deployment-id"),
        provider.to_string()
    );
    assert_eq!(
        response.header("x-serving-route-id"),
        usage
            .get::<_, Option<Uuid>>("served_route_id")
            .unwrap()
            .to_string()
    );
    let duplicate = db.pool().get().await.unwrap().query_one("SELECT r.was_inserted,(r.recorded_usage).served_route_id,(r.recorded_usage).served_deployment_id FROM (SELECT * FROM organization_usage_log WHERE model_name=$1 ORDER BY created_at DESC LIMIT 1) u CROSS JOIN LATERAL record_organization_usage(u.id,u.organization_id,u.workspace_id,u.api_key_id,u.model_id,u.model_name,u.input_tokens,u.output_tokens,u.cache_read_tokens,u.cache_write_tokens,u.input_cost,u.output_cost,u.total_cost,u.inference_type,u.ttft_ms,u.avg_itl_ms,u.inference_id,u.provider_request_id,u.stop_reason,u.response_id,u.image_count,u.served_provider_tier,u.served_provider_type,u.served_via_fallback,u.billing_details,u.service_tier,u.context_band,ARRAY[]::TEXT[],COALESCE(u.allocation_policy_version,'fixture')::VARCHAR,NULL::UUID,NULL::UUID) r", &[&name]).await.unwrap();
    assert!(!duplicate.get::<_, bool>("was_inserted"));
    assert_eq!(
        duplicate.get::<_, Option<Uuid>>("served_deployment_id"),
        Some(provider)
    );
    assert_eq!(
        duplicate.get::<_, Option<Uuid>>("served_route_id"),
        usage.get::<_, Option<Uuid>>("served_route_id")
    );

    let history = call(
        &server,
        Method::GET,
        &format!(
            "/v1/organizations/{}/usage/history?limit=20&offset=0",
            org.id
        ),
        json!({}),
    )
    .await;
    assert_eq!(history.status_code(), 200, "{}", history.text());
    let history = history.json::<Value>();
    let entry = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["model"] == name)
        .unwrap();
    assert_eq!(entry["served_deployment_id"], json!(provider));
    assert_eq!(
        entry["served_route_id"],
        json!(usage.get::<_, Option<Uuid>>("served_route_id").unwrap())
    );

    let response = call(
        &server,
        Method::PUT,
        &format!("/v1/admin/organizations/{}/inference-policy", org.id),
        json!({"tee_required":true,"change_reason":"Organization contract requires TEE"}),
    )
    .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&request)
        .await;
    assert_ne!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.status_code(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.json::<Value>()["error"]["type"], "bad_gateway");
}

#[tokio::test]
async fn provider_registry_audio_usage_keeps_serving_identity_and_accepted_price() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(path("/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(3))
                .set_body_json(json!({"text":"transport fixture", "duration":0.5})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let (server, _, _, db) = crate::common::setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 10
    })
    .await;
    let name = catalog(&server, true).await;
    let mut definition = definition();
    definition["config"]["base_url"] = json!(upstream.uri());
    let provider = create(&server, definition).await;
    approve(&server, provider, &name, true, false).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let route = repo.model_routes(&name).await.unwrap().remove(0);
    let response = call(&server, Method::POST,
        &format!("/v1/admin/provider-routes/{}/cost-profiles", route.id),
        json!({"rates":{"currency":"USD","billing_unit":"audio_second","input_microunits":7,"output_microunits":0,"provenance":"transport fixture"},"change_reason":"Audio procurement fixture"})).await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
    let org = crate::common::setup_org_with_credits(&server, 10_000_000_000).await;
    let key = crate::common::get_api_key_for_org(&server, org.id.clone()).await;
    let price_edited = std::sync::atomic::AtomicBool::new(false);
    let request = async {
        let response = server
            .post("/v1/audio/transcriptions")
            .add_header("Authorization", format!("Bearer {key}"))
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("model", name.clone())
                    .add_part(
                        "file",
                        axum_test::multipart::Part::bytes(vec![0u8; 128]).file_name("fixture.wav"),
                    ),
            )
            .await;
        assert!(
            price_edited.load(std::sync::atomic::Ordering::SeqCst),
            "price edit must finish before inference returns"
        );
        response
    };
    let edit_prices = async {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while upstream.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut batch = api::models::BatchUpdateModelApiRequest::new();
        batch.insert(
            name.clone(),
            serde_json::from_value(json!({
                "inputCostPerToken":{"amount":20_000_000,"currency":"USD"}
            }))
            .unwrap(),
        );
        admin_batch_upsert_models(&server, batch, get_session_id()).await;
        price_edited.store(true, std::sync::atomic::Ordering::SeqCst);
    };
    let (response, ()) = tokio::join!(request, edit_prices);
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response.header("x-serving-deployment-id"),
        provider.to_string()
    );
    assert_eq!(response.header("x-serving-route-id"), route.id.to_string());
    let usage = db.pool().get().await.unwrap().query_one("SELECT served_route_id,served_deployment_id,total_cost,billing_details FROM organization_usage_log WHERE model_name=$1", &[&name]).await.unwrap();
    assert_eq!(
        usage.get::<_, Option<Uuid>>("served_deployment_id"),
        Some(provider)
    );
    assert!(usage.get::<_, Option<Uuid>>("served_route_id").is_some());
    assert_eq!(usage.get::<_, i64>("total_cost"), 1_000_000);
    assert_eq!(
        usage.get::<_, Value>("billing_details")["accepted_catalog_pricing"]
            ["input_cost_per_token"],
        1_000_000
    );
    let attempt = db.pool().get().await.unwrap().query_one(
        "SELECT billable_quantity,usage_cost_microusd,actual_cost_microusd,cost_status FROM inference_attempts WHERE route_id=$1", &[&route.id]).await.unwrap();
    assert_eq!(
        attempt.get::<_, Value>("billable_quantity"),
        json!({"unit":"audio_second","microunits":500_000})
    );
    assert_eq!(
        attempt.get::<_, Option<i64>>("usage_cost_microusd"),
        Some(4)
    );
    assert_eq!(attempt.get::<_, Option<i64>>("actual_cost_microusd"), None);
    assert_eq!(attempt.get::<_, String>("cost_status"), "estimated");
}

#[tokio::test]
async fn provider_registry_responses_images_record_customer_and_procurement_usage() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(path("/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"image-fixture", "created":1, "data":[{"b64_json":"aW1hZ2U="}]
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (server, _, _, db) = crate::common::setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 10
    })
    .await;
    let name = catalog(&server, true).await;
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        name.clone(),
        serde_json::from_value(json!({
            "outputModalities":["image"],"inputModalities":["text"],
            "costPerImage":{"amount":123_000_000,"currency":"USD"}
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let mut definition = definition();
    definition["config"]["base_url"] = json!(upstream.uri());
    let provider = create(&server, definition).await;
    approve(&server, provider, &name, true, false).await;
    let repo = database::repositories::provider_registry::ProviderRegistryRepository::new(
        db.pool().clone(),
    );
    let route = repo.model_routes(&name).await.unwrap().remove(0);
    let response = call(&server, Method::POST,
        &format!("/v1/admin/provider-routes/{}/cost-profiles", route.id),
        json!({"rates":{"currency":"USD","billing_unit":"image","input_microunits":9,"output_microunits":0,"provenance":"transport fixture"},"change_reason":"Image procurement fixture"})).await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
    let org = crate::common::setup_org_with_credits(&server, 10_000_000_000).await;
    let key = crate::common::get_api_key_for_org(&server, org.id.clone()).await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({"model":name,"input":"Draw a tree","stream":false}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.json::<Value>()["status"], "completed");
    assert_eq!(
        response.json::<Value>()["output"][0]["metadata"]["serving"]["served_route_id"],
        route.id.to_string()
    );
    let usage = db.pool().get().await.unwrap().query_one(
        "SELECT served_route_id,total_cost,image_count,response_id FROM organization_usage_log WHERE model_name=$1", &[&name]).await.unwrap();
    assert_eq!(
        usage.get::<_, Option<Uuid>>("served_route_id"),
        Some(route.id)
    );
    assert_eq!(usage.get::<_, i64>("total_cost"), 123_000_000);
    assert_eq!(usage.get::<_, Option<i32>>("image_count"), Some(1));
    assert!(usage.get::<_, Option<Uuid>>("response_id").is_some());
    let attempt = db.pool().get().await.unwrap().query_one(
        "SELECT usage_cost_microusd,billable_quantity FROM inference_attempts WHERE route_id=$1", &[&route.id]).await.unwrap();
    assert_eq!(
        attempt.get::<_, Option<i64>>("usage_cost_microusd"),
        Some(9)
    );
    assert_eq!(
        attempt.get::<_, Value>("billable_quantity"),
        json!({"unit":"image","microunits":1_000_000})
    );
}

#[tokio::test]
async fn provider_registry_cancel_before_headers_records_unknown_cost_without_retry() {
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    Mock::given(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_delay(std::time::Duration::from_secs(30)))
        .expect(1)
        .mount(&upstream)
        .await;
    let (server, pool, _, db) = crate::common::setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 60;
    })
    .await;
    let name = catalog(&server, true).await;
    let mut definition = definition();
    definition["config"]["base_url"] = json!(upstream.uri());
    let provider = create(&server, definition).await;
    approve(&server, provider, &name, true, false).await;
    let params = serde_json::from_value(
        json!({"model":name,"messages":[{"role":"user","content":"fixture"}],"max_tokens":4}),
    )
    .unwrap();
    let request_id = Uuid::new_v4();
    let mut request = Box::pin(pool.chat_completion_with_attribution_and_hints(
        params,
        "fixture".into(),
        services::inference_provider_pool::ChatRoutingHints {
            request_id: Some(request_id),
            ..Default::default()
        },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                _ = &mut request => panic!("dispatch must still be waiting for upstream headers"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                    if !upstream.received_requests().await.unwrap().is_empty() { break; }
                }
            }
        }
    })
    .await
    .unwrap();
    drop(request);
    let row = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = db.pool().get().await.unwrap().query_one(
                "SELECT outcome,cost_status,actual_cost_microusd FROM inference_attempts WHERE accepted_request_id=$1", &[&request_id]).await.unwrap();
            if row.get::<_, String>("outcome") == "cancelled" { break row; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    assert_eq!(row.get::<_, String>("cost_status"), "unknown");
    assert_eq!(row.get::<_, Option<i64>>("actual_cost_microusd"), None);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
