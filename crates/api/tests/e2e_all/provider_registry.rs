//! Management E2E: real HTTP handlers, PostgreSQL, migrations and external
//! provider registration. No upstream inference results are fabricated.
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
        let response = call(
            &server,
            Method::PUT,
            &format!("/v1/admin/models/{name}/archive"),
            json!({"archived":false}),
        )
        .await;
        assert_eq!(response.status_code(), 204, "{}", response.text());
        let after = repo.get_by_internal_name(&name).await.unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.input_cost_per_token, before.input_cost_per_token);
        assert_eq!(after.is_active, active);
        assert_eq!(pool.has_provider(&name).await, active);
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
