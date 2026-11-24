//! End-to-end tests for model routes: a third-party model whose route names
//! another catalog entry as an extra provider.

use std::sync::Arc;

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use database::repositories::model_route::query_model_routes;
use database::repositories::PgModelRouteRepository;
use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate};
use inference_providers::{CompletionError, InferenceProvider};
use services::inference_provider_pool::{InferenceProviderPool, ModelRoute};

const HEADER_SERVING_ROUTE: &str = "x-serving-route";

const MODEL_INPUT_COST: i64 = 1_000;
const MODEL_OUTPUT_COST: i64 = 2_000;

fn external_model(display_name: &str) -> serde_json::Value {
    priced_external_model(display_name, MODEL_INPUT_COST, MODEL_OUTPUT_COST)
}

fn priced_external_model(
    display_name: &str,
    input_cost: i64,
    output_cost: i64,
) -> serde_json::Value {
    serde_json::json!({
        "inputCostPerToken":  { "amount": input_cost, "currency": "USD" },
        "outputCostPerToken": { "amount": output_cost, "currency": "USD" },
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

/// A provider that answers every chat request with `content`.
async fn answering(content: &str) -> Arc<MockProvider> {
    let provider = Arc::new(MockProvider::new_accept_all());
    provider
        .when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new(content))
        .await;
    provider
}

/// A model and the catalog entry its route targets, each served by its own
/// provider, with the route loaded into the pool.
async fn routed_model(
    server: &axum_test::TestServer,
    pool: &InferenceProviderPool,
) -> (String, ModelRoute, Arc<MockProvider>, Arc<MockProvider>) {
    let model = format!("e2e-vendor/routed-{}", uuid::Uuid::new_v4());
    let target = format!("e2e-vendor/routed-target-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(external_model("Routed model")).unwrap(),
    );
    // Priced apart from the model, so a charge shows which entry priced it.
    batch.insert(
        target.clone(),
        serde_json::from_value(priced_external_model("Route target", 70_000, 90_000)).unwrap(),
    );
    seed_models(server, batch, get_session_id()).await;

    let own = answering("served-by-own").await;
    let routed = answering("served-by-route").await;
    pool.register_provider(
        model.clone(),
        own.clone() as Arc<dyn InferenceProvider + Send + Sync>,
    )
    .await;
    pool.register_provider(
        target.clone(),
        routed.clone() as Arc<dyn InferenceProvider + Send + Sync>,
    )
    .await;

    let route = ModelRoute {
        id: uuid::Uuid::new_v4(),
        model: model.clone(),
        target,
        priority: 100,
    };
    pool.set_model_routes(vec![route.clone()]);
    (model, route, own, routed)
}

fn unavailable() -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: "upstream unavailable".to_string(),
        is_external: true,
    }
}

async fn chat(
    server: &axum_test::TestServer,
    api_key: &str,
    model: &str,
    stream: bool,
) -> axum_test::TestResponse {
    server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "stream": stream
        }))
        .await
}

fn serving_route(response: &axum_test::TestResponse) -> Option<String> {
    response
        .headers()
        .get(HEADER_SERVING_ROUTE)
        .map(|value| value.to_str().unwrap().to_string())
}

/// While the model's own provider answers, it serves and no route is named.
#[tokio::test]
async fn e2e_the_models_own_provider_serves_and_names_no_route() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let (model, _route, own, routed) = routed_model(&server, &pool).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = chat(&server, &api_key, &model, false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-own"));
    assert_eq!(serving_route(&response), None);

    let streamed = chat(&server, &api_key, &model, true).await;

    assert_eq!(streamed.status_code(), 200, "{}", streamed.text());
    assert!(streamed.text().contains("served-by-own"));
    assert_eq!(serving_route(&streamed), None);
    assert_eq!(own.chat_completion_call_count(), 2);
    assert_eq!(routed.chat_completion_call_count(), 0);
}

/// When the model's own provider fails, the route target serves the same
/// request, the response names the route, and the usage is recorded and
/// priced on the requested model.
#[tokio::test]
async fn e2e_a_route_target_serves_when_the_models_own_provider_fails() {
    let (server, pool, _mock, database) = setup_test_server_with_pool().await;
    let (model, route, own, routed) = routed_model(&server, &pool).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let organization_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let api_key = get_api_key_for_org(&server, org.id).await;
    own.set_error_override(Some(unavailable())).await;

    let response = chat(&server, &api_key, &model, false).await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(response.text().contains("served-by-route"));
    assert_eq!(serving_route(&response), Some(route.id.to_string()));
    assert_eq!(
        response.headers().get("x-serving-provider").unwrap(),
        "non-attested"
    );
    assert_eq!(routed.chat_completion_call_count(), 1);

    let client = database.pool().get().await.unwrap();
    let mut usage = None;
    for _ in 0..50 {
        usage = client
            .query_opt(
                "SELECT model_name, served_via_fallback, input_tokens, output_tokens, total_cost
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
    assert_eq!(usage.get::<_, String>("model_name"), model);
    assert!(usage.get::<_, bool>("served_via_fallback"));
    let input_tokens = i64::from(usage.get::<_, i32>("input_tokens"));
    let output_tokens = i64::from(usage.get::<_, i32>("output_tokens"));
    assert!(input_tokens > 0 && output_tokens > 0);
    assert_eq!(
        usage.get::<_, i64>("total_cost"),
        input_tokens * MODEL_INPUT_COST + output_tokens * MODEL_OUTPUT_COST,
        "priced on the requested model, not the route target"
    );

    let streamed = chat(&server, &api_key, &model, true).await;

    assert_eq!(streamed.status_code(), 200, "{}", streamed.text());
    assert!(streamed.text().contains("served-by-route"));
    assert_eq!(serving_route(&streamed), Some(route.id.to_string()));
    assert_eq!(routed.chat_completion_call_count(), 2);
}

/// The repository reads enabled routes between active third-party entries,
/// in route order, and maps a schema without the routes table (SQLSTATE
/// 42P01) to "no routes", not an error.
#[tokio::test]
async fn e2e_repository_reads_enabled_routes_and_maps_a_missing_table_to_no_routes() {
    let (server, database) = setup_test_server_with_database().await;
    let model = format!("e2e-vendor/routes-{}", uuid::Uuid::new_v4());
    let first = format!("e2e-vendor/routes-first-{}", uuid::Uuid::new_v4());
    let second = format!("e2e-vendor/routes-second-{}", uuid::Uuid::new_v4());
    let disabled = format!("e2e-vendor/routes-disabled-{}", uuid::Uuid::new_v4());
    let hosted = format!("e2e-vendor/routes-hosted-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    for name in [&model, &first, &second, &disabled] {
        batch.insert(
            name.clone(),
            serde_json::from_value(external_model("Routes repository e2e")).unwrap(),
        );
    }
    batch.insert(
        hosted.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000, "currency": "USD" },
            "modelDisplayName":   "Routes repository e2e (hosted)",
            "modelDescription":   "Synthetic hosted model",
            "contextLength":      4_096,
            "maxOutputLength":    1_024,
            "verifiable":         true,
            "isActive":           true,
        }))
        .unwrap(),
    );
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
                     priority INTEGER NOT NULL DEFAULT 100,
                     is_enabled BOOLEAN NOT NULL DEFAULT FALSE
                 );"
            ))
            .await
            .unwrap();
    }
    for (target, priority, enabled) in [
        (&first, 20, true),
        (&second, 10, true),
        (&disabled, 5, false),
        (&hosted, 1, true),
    ] {
        transaction
            .execute(
                "INSERT INTO model_routes (model_id, target_model_id, priority, is_enabled)
                 SELECT m.id, t.id, $3, $4 FROM models m, models t
                 WHERE m.model_name = $1 AND t.model_name = $2",
                &[&model, target, &priority, &enabled],
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
        .map(|route| (route.target, route.priority))
        .collect();
    assert_eq!(routes, vec![(second, 10), (first, 20)]);
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
