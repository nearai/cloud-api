//! End-to-end tests for model routes: a model whose route gives another
//! catalog entry a provider name, and chat requests that name it or not.

use std::sync::Arc;

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use database::repositories::model_route::query_model_routes;
use database::repositories::PgModelRouteRepository;
use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
use inference_providers::{CompletionError, InferenceProvider};
use services::inference_provider_pool::{InferenceProviderPool, ModelRoute, ModelRoutesSource};

const HEADER_MODEL_PROVIDER: &str = "x-model-provider";
const PROVIDER: &str = "other";

const TARGET_INPUT_COST: i64 = 70_000;
const TARGET_OUTPUT_COST: i64 = 90_000;

fn hosted_model(display_name: &str) -> serde_json::Value {
    serde_json::json!({
        "inputCostPerToken":  { "amount": 1_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000, "currency": "USD" },
        "modelDisplayName":   display_name,
        "modelDescription":   "Synthetic hosted model",
        "contextLength":      128_000,
        "maxOutputLength":    1_024,
        "verifiable":         true,
        "isActive":           true,
    })
}

fn third_party_model(display_name: &str) -> serde_json::Value {
    serde_json::json!({
        "inputCostPerToken":  { "amount": TARGET_INPUT_COST, "currency": "USD" },
        "outputCostPerToken": { "amount": TARGET_OUTPUT_COST, "currency": "USD" },
        "modelDisplayName":   display_name,
        "modelDescription":   "Synthetic third-party model",
        "contextLength":      128_000,
        "maxOutputLength":    1_024,
        "verifiable":         false,
        "isActive":           true,
        "providerType":       "external",
        "providerConfig": {
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1",
            "model_name": "upstream-model",
        },
        "attestationSupported": false
    })
}

/// `model` with the fields of `more` added.
fn with(mut model: serde_json::Value, more: serde_json::Value) -> serde_json::Value {
    for (key, value) in more.as_object().unwrap() {
        model[key] = value.clone();
    }
    model
}

async fn seed(server: &axum_test::TestServer, models: Vec<(&str, serde_json::Value)>) {
    let mut batch = BatchUpdateModelApiRequest::new();
    for (name, model) in models {
        batch.insert(name.to_string(), serde_json::from_value(model).unwrap());
    }
    seed_models(server, batch, get_session_id()).await;
}

async fn serve(pool: &InferenceProviderPool, model: &str, content: &str) -> Arc<MockProvider> {
    let provider = answering(content).await;
    pool.register_provider(
        model.to_string(),
        provider.clone() as Arc<dyn InferenceProvider + Send + Sync>,
    )
    .await;
    provider
}

fn route(model: &str, provider: &str, target: &str) -> ModelRoute {
    ModelRoute {
        id: uuid::Uuid::new_v4(),
        model: model.to_string(),
        provider: provider.to_string(),
        target: target.to_string(),
    }
}

/// A provider that answers every chat request with `content`.
async fn answering(content: &str) -> Arc<MockProvider> {
    let provider = Arc::new(MockProvider::new_accept_all());
    provider
        .when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new(content))
        .await;
    provider
}

struct Routed {
    model: String,
    target: String,
    own: Arc<MockProvider>,
    other: Arc<MockProvider>,
}

/// A hosted model and a third-party catalog entry, each served by its own
/// provider, with a route that names the entry `PROVIDER` under the model.
async fn routed_model(server: &axum_test::TestServer, pool: &InferenceProviderPool) -> Routed {
    let model = format!("e2e-vendor/routed-{}", uuid::Uuid::new_v4());
    let target = format!("e2e-vendor/routed-target-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(hosted_model("Routed model")).unwrap(),
    );
    batch.insert(
        target.clone(),
        serde_json::from_value(third_party_model("Route target")).unwrap(),
    );
    seed_models(server, batch, get_session_id()).await;

    let own = answering("served-by-own").await;
    let other = answering("served-by-other").await;
    pool.register_provider(
        model.clone(),
        own.clone() as Arc<dyn InferenceProvider + Send + Sync>,
    )
    .await;
    pool.register_provider(
        target.clone(),
        other.clone() as Arc<dyn InferenceProvider + Send + Sync>,
    )
    .await;
    pool.set_model_routes(vec![ModelRoute {
        id: uuid::Uuid::new_v4(),
        model: model.clone(),
        provider: PROVIDER.to_string(),
        target: target.clone(),
    }]);
    Routed {
        model,
        target,
        own,
        other,
    }
}

async fn chat(
    server: &axum_test::TestServer,
    api_key: &str,
    model: &str,
    provider: Option<serde_json::Value>,
    stream: bool,
) -> axum_test::TestResponse {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Hi"}],
        "max_tokens": 10,
        "stream": stream
    });
    if let Some(provider) = provider {
        body["provider"] = provider;
    }
    server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .await
}

fn model_provider(response: &axum_test::TestResponse) -> Option<String> {
    response
        .headers()
        .get(HEADER_MODEL_PROVIDER)
        .map(|value| value.to_str().unwrap().to_string())
}

async fn api_key(server: &axum_test::TestServer) -> (uuid::Uuid, String) {
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    let organization_id = uuid::Uuid::parse_str(&org.id).unwrap();
    (organization_id, get_api_key_for_org(server, org.id).await)
}

/// Without a provider named, the model's own provider serves, streaming or
/// not, and the response names no provider.
#[tokio::test]
async fn e2e_a_model_with_routes_serves_from_its_own_provider_by_default() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (_, key) = api_key(&server).await;

    for stream in [false, true] {
        let response = chat(&server, &key, &routed.model, None, stream).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(response.text().contains("served-by-own"));
        assert_eq!(model_provider(&response), None);
    }
    // Other provider fields, and an empty `only`, name no provider.
    for provider in [
        serde_json::json!({"sort": "price"}),
        serde_json::json!({"only": []}),
        serde_json::json!({"only": null}),
    ] {
        let response = chat(&server, &key, &routed.model, Some(provider), false).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(response.text().contains("served-by-own"));
        assert_eq!(model_provider(&response), None);
    }

    assert_eq!(routed.own.chat_completion_call_count(), 5);
    assert_eq!(routed.other.chat_completion_call_count(), 0);
}

/// A request that names a provider is served by that provider's catalog
/// entry: its provider answers, the response names it, and the usage is
/// recorded and priced on that entry.
#[tokio::test]
async fn e2e_a_named_provider_serves_the_request_as_its_catalog_entry() {
    let (server, pool, _mock, database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (organization_id, key) = api_key(&server).await;
    let named = serde_json::json!({"only": [PROVIDER], "sort": "price"});

    let response = chat(&server, &key, &routed.model, Some(named.clone()), false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-other"));
    assert_eq!(model_provider(&response), Some(PROVIDER.to_string()));
    // The answer is the entry's: not attested, although the model is.
    assert_eq!(
        response.headers().get("x-serving-provider").unwrap(),
        "non-attested"
    );
    assert_eq!(routed.own.chat_completion_call_count(), 0);
    assert_eq!(routed.other.chat_completion_call_count(), 1);

    // The entry's provider got a request for the entry, without the name
    // that picked it; the caller's other provider fields are kept.
    let sent = routed.other.last_chat_params().await.expect("a request");
    assert_eq!(sent.model, routed.target);
    assert_eq!(
        sent.extra.get("provider"),
        Some(&serde_json::json!({"sort": "price"}))
    );

    let client = database.pool().get().await.unwrap();
    let mut usage = None;
    for _ in 0..50 {
        usage = client
            .query_opt(
                "SELECT model_name, input_tokens, output_tokens, total_cost
                 FROM organization_usage_log WHERE organization_id = $1",
                &[&organization_id],
            )
            .await
            .unwrap();
        if usage.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let usage = usage.expect("the request's usage is recorded");
    assert_eq!(usage.get::<_, String>("model_name"), routed.target);
    let input_tokens = i64::from(usage.get::<_, i32>("input_tokens"));
    let output_tokens = i64::from(usage.get::<_, i32>("output_tokens"));
    assert!(input_tokens > 0 && output_tokens > 0);
    assert_eq!(
        usage.get::<_, i64>("total_cost"),
        input_tokens * TARGET_INPUT_COST + output_tokens * TARGET_OUTPUT_COST,
        "priced on the named provider's entry"
    );

    let streamed = chat(&server, &key, &routed.model, Some(named), true).await;

    assert_eq!(streamed.status_code(), 200, "{}", streamed.text());
    assert!(streamed.text().contains("served-by-other"));
    assert_eq!(model_provider(&streamed), Some(PROVIDER.to_string()));
    assert_eq!(routed.own.chat_completion_call_count(), 0);
}

/// A named provider that fails is not replaced by the model's own.
#[tokio::test]
async fn e2e_a_named_provider_that_fails_is_not_replaced() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (_, key) = api_key(&server).await;
    routed
        .other
        .set_error_override(Some(CompletionError::HttpError {
            status_code: 503,
            message: "upstream unavailable".to_string(),
            is_external: true,
        }))
        .await;

    let response = chat(
        &server,
        &key,
        &routed.model,
        Some(serde_json::json!({"only": [PROVIDER]})),
        false,
    )
    .await;

    // The provider's failure is the answer (an unavailable upstream is
    // reported as overload).
    assert_eq!(response.status_code(), 429, "{}", response.text());
    assert!(!response.text().contains("served-by-own"));
    assert!(routed.other.chat_completion_call_count() > 0);
    assert_eq!(routed.own.chat_completion_call_count(), 0);
}

/// A name the model does not have is refused with the names it has, before
/// any provider is called.
#[tokio::test]
async fn e2e_an_unknown_provider_name_is_refused_with_the_valid_names() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (_, key) = api_key(&server).await;

    for only in [
        serde_json::json!(["nope"]),
        serde_json::json!([PROVIDER, "nope"]),
        serde_json::json!(PROVIDER),
    ] {
        let response = chat(
            &server,
            &key,
            &routed.model,
            Some(serde_json::json!({"only": only})),
            false,
        )
        .await;
        assert_eq!(response.status_code(), 400, "{}", response.text());
        let body: serde_json::Value = response.json();
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("has these providers: {PROVIDER}.")),
            "{message}"
        );
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
    assert_eq!(routed.own.chat_completion_call_count(), 0);
    assert_eq!(routed.other.chat_completion_call_count(), 0);
}

/// A model without routes keeps today's behaviour: `provider.only` is passed
/// to its own provider untouched, also while other models have routes.
#[tokio::test]
async fn e2e_a_model_without_routes_passes_provider_only_through() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (_, key) = api_key(&server).await;
    let named = serde_json::json!({"only": [PROVIDER]});

    // The route's target has no routes of its own.
    let response = chat(&server, &key, &routed.target, Some(named.clone()), false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-other"));
    assert_eq!(model_provider(&response), None);
    let sent = routed.other.last_chat_params().await.expect("a request");
    assert_eq!(sent.extra.get("provider"), Some(&named));
    assert_eq!(routed.own.chat_completion_call_count(), 0);

    // With no route loaded at all, the routed model behaves the same way.
    pool.set_model_routes(Vec::new());
    let response = chat(&server, &key, &routed.model, Some(named.clone()), false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-own"));
    assert_eq!(model_provider(&response), None);
    let sent = routed.own.last_chat_params().await.expect("a request");
    assert_eq!(sent.extra.get("provider"), Some(&named));
}

/// A source of routes that cannot be read.
struct Unreadable;

#[async_trait::async_trait]
impl ModelRoutesSource for Unreadable {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String> {
        Err("unavailable".to_string())
    }
}

/// Routes that have never loaded are unknown, not absent: a request that
/// names a provider fails instead of being served by the default provider,
/// and a request that names none is served as usual.
#[tokio::test]
async fn e2e_naming_a_provider_fails_until_the_routes_have_loaded() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let model = format!("e2e-vendor/unloaded-{}", uuid::Uuid::new_v4());
    seed(&server, vec![(&model, hosted_model("Unloaded routes"))]).await;
    let own = serve(&pool, &model, "served-by-own").await;
    let (_, key) = api_key(&server).await;
    pool.set_model_routes_source(Arc::new(Unreadable));
    pool.refresh_model_routes().await;

    let named = serde_json::json!({"only": [PROVIDER]});
    let response = chat(&server, &key, &model, Some(named), false).await;

    assert_eq!(response.status_code(), 500, "{}", response.text());
    assert!(response.text().contains("provider.only"));
    assert_eq!(model_provider(&response), None);
    assert_eq!(own.chat_completion_call_count(), 0);

    let response = chat(&server, &key, &model, None, false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-own"));
}

/// A route is followed once: the target's own route of the same name is not.
#[tokio::test]
async fn e2e_a_named_provider_is_followed_one_hop() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let routed = routed_model(&server, &pool).await;
    let (_, key) = api_key(&server).await;
    pool.set_model_routes(vec![
        route(&routed.model, PROVIDER, &routed.target),
        route(&routed.target, PROVIDER, &routed.model),
    ]);

    let response = chat(
        &server,
        &key,
        &routed.model,
        Some(serde_json::json!({"only": [PROVIDER]})),
        false,
    )
    .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-other"));
    assert_eq!(routed.own.chat_completion_call_count(), 0);
}

/// A target the caller may not request is not theirs to learn about: its
/// provider name is not offered, and naming it reveals nothing.
#[tokio::test]
async fn e2e_a_target_the_caller_cannot_use_is_not_offered() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let model = format!("e2e-vendor/routed-{}", uuid::Uuid::new_v4());
    let open = format!("e2e-vendor/routed-open-{}", uuid::Uuid::new_v4());
    let hidden = format!("e2e-vendor/routed-hidden-{}", uuid::Uuid::new_v4());
    seed(
        &server,
        vec![
            (&model, hosted_model("Routed model")),
            (&open, third_party_model("Open target")),
            (
                &hidden,
                with(
                    third_party_model("Hidden target"),
                    serde_json::json!({"isListed": false}),
                ),
            ),
        ],
    )
    .await;
    let own = serve(&pool, &model, "served-by-own").await;
    let via_open = serve(&pool, &open, "served-by-open").await;
    let via_hidden = serve(&pool, &hidden, "served-by-hidden").await;
    let (_, key) = api_key(&server).await;

    // One usable route, one not: only the usable name is offered.
    pool.set_model_routes(vec![
        route(&model, "open", &open),
        route(&model, "hidden", &hidden),
    ]);
    let response = chat(
        &server,
        &key,
        &model,
        Some(serde_json::json!({"only": ["hidden"]})),
        false,
    )
    .await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let text = response.text();
    assert!(text.contains("has these providers: open."), "{text}");
    assert!(!text.contains("hidden"), "{text}");

    // No usable route: a name is still refused, with none listed, and the
    // model's own provider is not handed a name that is not its own.
    pool.set_model_routes(vec![route(&model, "hidden", &hidden)]);
    let named = serde_json::json!({"only": ["hidden"]});
    let response = chat(&server, &key, &model, Some(named), false).await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let text = response.text();
    assert!(text.contains("has no provider that can be named"), "{text}");
    assert!(!text.contains("hidden"), "{text}");
    assert_eq!(model_provider(&response), None);
    assert_eq!(own.chat_completion_call_count(), 0);

    // Without a name the model serves as usual.
    let response = chat(&server, &key, &model, None, false).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-own"));

    assert_eq!(via_open.chat_completion_call_count(), 0);
    assert_eq!(via_hidden.chat_completion_call_count(), 0);
}

/// A model the caller may not request answers as an unknown model, whether
/// or not a provider is named: its routes are not revealed.
#[tokio::test]
async fn e2e_a_model_the_caller_cannot_use_reveals_no_routes() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let model = format!("e2e-vendor/routed-hidden-model-{}", uuid::Uuid::new_v4());
    let target = format!("e2e-vendor/routed-target-{}", uuid::Uuid::new_v4());
    seed(
        &server,
        vec![
            (
                &model,
                with(
                    hosted_model("Hidden routed model"),
                    serde_json::json!({"isListed": false}),
                ),
            ),
            (&target, third_party_model("Route target")),
        ],
    )
    .await;
    let own = serve(&pool, &model, "served-by-own").await;
    let other = serve(&pool, &target, "served-by-other").await;
    pool.set_model_routes(vec![route(&model, PROVIDER, &target)]);
    let (_, key) = api_key(&server).await;

    let plain = chat(&server, &key, &model, None, false).await;
    let named = chat(
        &server,
        &key,
        &model,
        Some(serde_json::json!({"only": [PROVIDER]})),
        false,
    )
    .await;

    assert!(plain.status_code().is_client_error(), "{}", plain.text());
    assert_eq!(named.status_code(), plain.status_code());
    assert_eq!(named.text(), plain.text());
    assert_eq!(own.chat_completion_call_count(), 0);
    assert_eq!(other.chat_completion_call_count(), 0);
}

/// Naming a provider through an alias of the model is refused with the
/// canonical name; an alias never resolves silently.
#[tokio::test]
async fn e2e_naming_a_provider_through_an_alias_is_refused() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let model = format!("e2e-vendor/routed-{}", uuid::Uuid::new_v4());
    let alias = format!("e2e-routed-alias-{}", uuid::Uuid::new_v4());
    let target = format!("e2e-vendor/routed-target-{}", uuid::Uuid::new_v4());
    seed(
        &server,
        vec![
            (
                &model,
                with(
                    hosted_model("Routed model"),
                    serde_json::json!({"aliases": [alias]}),
                ),
            ),
            (&target, third_party_model("Route target")),
        ],
    )
    .await;
    let own = serve(&pool, &model, "served-by-own").await;
    let other = serve(&pool, &target, "served-by-other").await;
    pool.set_model_routes(vec![route(&model, PROVIDER, &target)]);
    let (_, key) = api_key(&server).await;

    let response = chat(
        &server,
        &key,
        &alias,
        Some(serde_json::json!({"only": [PROVIDER]})),
        false,
    )
    .await;

    assert_eq!(response.status_code(), 400, "{}", response.text());
    let body: serde_json::Value = response.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("Use the canonical model name '{model}'")),
        "{message}"
    );
    assert_eq!(own.chat_completion_call_count(), 0);
    assert_eq!(other.chat_completion_call_count(), 0);

    // Without a provider named, the alias works as before.
    let response = chat(&server, &key, &alias, None, false).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-own"));
}

/// The repository reads the enabled routes between active catalog entries,
/// and maps a schema without the routes table (SQLSTATE 42P01) to "no
/// routes", not an error.
#[tokio::test]
async fn e2e_repository_reads_enabled_routes_and_maps_a_missing_table_to_no_routes() {
    let (server, database) = setup_test_server_with_database().await;
    let model = format!("e2e-vendor/routes-{}", uuid::Uuid::new_v4());
    let first = format!("e2e-vendor/routes-first-{}", uuid::Uuid::new_v4());
    let second = format!("e2e-vendor/routes-second-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(hosted_model("Routes repository e2e")).unwrap(),
    );
    for name in [&first, &second] {
        batch.insert(
            name.clone(),
            serde_json::from_value(third_party_model("Routes repository e2e")).unwrap(),
        );
    }
    seed_models(&server, batch, get_session_id()).await;

    // The routes table when the schema has it; otherwise the columns the query
    // reads, in a schema of this transaction only. Nothing is committed.
    let mut client = database.pool().get().await.unwrap();
    let transaction = client.transaction().await.unwrap();
    let has_table: bool = transaction
        .query_one("SELECT to_regclass('public.model_routes') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    if !has_table {
        let schema = format!("e2e_model_routes_{}", uuid::Uuid::new_v4().simple());
        transaction
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 SET LOCAL search_path TO {schema}, public;
                 CREATE TABLE model_routes (
                     id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                     model_id UUID NOT NULL,
                     target_model_id UUID NOT NULL,
                     provider VARCHAR(32) NOT NULL,
                     is_enabled BOOLEAN NOT NULL DEFAULT FALSE
                 );"
            ))
            .await
            .unwrap();
    }
    for (provider, target, enabled) in [
        ("zeta", &first, true),
        ("alpha", &second, true),
        ("off", &first, false),
    ] {
        transaction
            .execute(
                "INSERT INTO model_routes (model_id, target_model_id, provider, is_enabled)
                 SELECT m.id, t.id, $3, $4 FROM models m, models t
                 WHERE m.model_name = $1 AND t.model_name = $2",
                &[&model, target, &provider, &enabled],
            )
            .await
            .unwrap();
    }

    let routes: Vec<_> = query_model_routes(&*transaction)
        .await
        .unwrap()
        .expect("the routes table is visible")
        .into_iter()
        .filter(|route| route.model == model)
        .map(|route| (route.provider, route.target))
        .collect();
    assert_eq!(
        routes,
        vec![("alpha".to_string(), second), ("zeta".to_string(), first)]
    );
    drop(transaction);

    // Connections that cannot see the table: no routes.
    let mut client = database.pool().get().await.unwrap();
    let transaction = client.transaction().await.unwrap();
    transaction
        .batch_execute("SET LOCAL search_path TO e2e_schema_without_tables")
        .await
        .unwrap();
    assert_eq!(query_model_routes(&*transaction).await.unwrap(), None);
    drop(transaction);

    let isolated = PgModelRouteRepository::new(
        crate::common::db_setup::create_test_pool_with_search_path("e2e_schema_without_tables")
            .await,
    );
    assert_eq!(isolated.list_enabled().await.unwrap(), Vec::new());
}
