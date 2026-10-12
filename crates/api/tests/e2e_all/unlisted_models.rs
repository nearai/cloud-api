// E2E tests for unlisted models (`isListed: false`).
//
// An unlisted model is an active, priced catalog row that the public catalog
// endpoints leave out. Trusted services read it from `GET /v1/internal/models`
// and bill usage against it through `POST /v1/internal/usage`.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

const INTERNAL_TOKEN: &str = "unlisted-models-internal-secret";

async fn server_with_internal_token() -> axum_test::TestServer {
    setup_test_server_with_config(|config| {
        config.internal_usage_token = Some(INTERNAL_TOKEN.to_string());
    })
    .await
}

fn unique_model_name(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// Create or update one model through `PATCH /v1/admin/models` and return its
/// entry from the response.
async fn upsert_model(
    server: &axum_test::TestServer,
    model_name: &str,
    fields: serde_json::Value,
) -> serde_json::Value {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.to_string(),
        serde_json::from_value(fields).expect("valid model update"),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    let model = updated
        .into_iter()
        .find(|model| model.model_id == model_name)
        .expect("the upserted model is in the response");
    serde_json::to_value(model).expect("model serializes")
}

/// A priced, active row with no inference URL: what a lane-only model looks like.
fn new_model_fields(is_listed: Option<bool>) -> serde_json::Value {
    let mut fields = serde_json::json!({
        "inputCostPerToken": { "amount": 100, "currency": "USD" },
        "outputCostPerToken": { "amount": 500, "currency": "USD" },
        "modelDisplayName": "Unlisted Test Model",
        "modelDescription": "A model used by the unlisted-model e2e tests",
        "contextLength": 131072,
        "maxOutputLength": 32768,
        "verifiable": false,
        "attestationSupported": false,
        "isActive": true
    });
    if let Some(is_listed) = is_listed {
        fields["isListed"] = serde_json::json!(is_listed);
    }
    fields
}

/// Ids on the public, unauthenticated `GET /v1/models`.
async fn public_model_ids(server: &axum_test::TestServer) -> Vec<String> {
    let response = server.get("/v1/models").await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response
        .json::<api::models::ModelsResponse>()
        .data
        .into_iter()
        .map(|model| model.id)
        .collect()
}

/// Ids on the public, unauthenticated `GET /v1/model/list`.
async fn public_model_list_ids(server: &axum_test::TestServer) -> Vec<String> {
    let response = server.get("/v1/model/list?limit=100000").await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    body["models"]
        .as_array()
        .expect("models is an array")
        .iter()
        .map(|model| model["modelId"].as_str().expect("modelId").to_string())
        .collect()
}

async fn public_model_detail_status(server: &axum_test::TestServer, identifier: &str) -> u16 {
    server
        .get(&format!("/v1/model/{identifier}"))
        .await
        .status_code()
        .as_u16()
}

/// Entries of `GET /v1/internal/models`, read with the service token.
async fn internal_models(server: &axum_test::TestServer) -> Vec<serde_json::Value> {
    let response = server
        .get("/v1/internal/models")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.header("cache-control"), "no-store");
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "list");
    body["data"].as_array().expect("data is an array").clone()
}

fn find_model<'a>(models: &'a [serde_json::Value], id: &str) -> Option<&'a serde_json::Value> {
    models.iter().find(|model| model["id"] == id)
}

/// `isListed` on the newest entry of the model's admin history.
async fn latest_history_is_listed(server: &axum_test::TestServer, model_name: &str) -> bool {
    let response = server
        .get(&format!("/v1/admin/models/{model_name}/history"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let history: serde_json::Value = response.json();
    history["history"][0]["isListed"]
        .as_bool()
        .expect("history entries carry isListed")
}

#[tokio::test]
async fn unlisted_model_is_left_out_of_the_public_catalogs() {
    let server = server_with_internal_token().await;
    let model_name = unique_model_name("unlisted-model");
    let alias = unique_model_name("unlisted-alias");

    let mut fields = new_model_fields(Some(false));
    fields["aliases"] = serde_json::json!([alias]);
    let created = upsert_model(&server, &model_name, fields).await;
    assert_eq!(created["metadata"]["isListed"], false);

    assert!(!public_model_ids(&server).await.contains(&model_name));
    assert!(!public_model_list_ids(&server).await.contains(&model_name));
    assert_eq!(public_model_detail_status(&server, &model_name).await, 404);
    assert_eq!(public_model_detail_status(&server, &alias).await, 404);

    // The internal document carries it, with the public document's shape.
    let internal = internal_models(&server).await;
    let entry = find_model(&internal, &model_name).expect("unlisted model on the internal list");
    assert_eq!(entry["pricing"]["prompt"], "0.0000001");
    assert_eq!(entry["pricing"]["completion"], "0.0000005");
    assert_eq!(entry["context_length"], 131072);
    assert_eq!(entry["max_output_length"], 32768);

    // Admins see the switch on the model and in its history.
    let admin_models = list_all_admin_models(&server, false).await;
    let admin_entry = admin_models
        .iter()
        .find(|model| model.model_id == model_name)
        .expect("unlisted model on the admin list");
    assert_eq!(admin_entry.metadata.is_listed, Some(false));

    assert!(!latest_history_is_listed(&server, &model_name).await);
}

#[tokio::test]
async fn models_are_listed_unless_told_otherwise() {
    let server = server_with_internal_token().await;
    let model_name = unique_model_name("listed-by-default");

    let created = upsert_model(&server, &model_name, new_model_fields(None)).await;
    assert_eq!(created["metadata"]["isListed"], true);

    assert!(public_model_ids(&server).await.contains(&model_name));
    assert!(public_model_list_ids(&server).await.contains(&model_name));
    assert_eq!(public_model_detail_status(&server, &model_name).await, 200);
    assert!(find_model(&internal_models(&server).await, &model_name).is_some());

    // The public documents never carry the admin-only field.
    let detail: serde_json::Value = server.get(&format!("/v1/model/{model_name}")).await.json();
    assert!(detail["metadata"].get("isListed").is_none());
}

#[tokio::test]
async fn listing_state_changes_only_when_the_field_is_sent() {
    let server = server_with_internal_token().await;
    let model_name = unique_model_name("unlisted-toggle");

    upsert_model(&server, &model_name, new_model_fields(Some(false))).await;

    // A partial update that does not mention the field keeps the model unlisted.
    let repriced = upsert_model(
        &server,
        &model_name,
        serde_json::json!({ "outputCostPerToken": { "amount": 600, "currency": "USD" } }),
    )
    .await;
    assert_eq!(repriced["metadata"]["isListed"], false);
    assert_eq!(repriced["outputCostPerToken"]["amount"], 600);
    assert!(!public_model_ids(&server).await.contains(&model_name));
    assert!(!latest_history_is_listed(&server, &model_name).await);

    // Listing it publishes it at once on this instance.
    let listed = upsert_model(
        &server,
        &model_name,
        serde_json::json!({ "isListed": true }),
    )
    .await;
    assert_eq!(listed["metadata"]["isListed"], true);
    assert!(latest_history_is_listed(&server, &model_name).await);
    assert!(public_model_ids(&server).await.contains(&model_name));
    assert!(public_model_list_ids(&server).await.contains(&model_name));
    assert_eq!(public_model_detail_status(&server, &model_name).await, 200);

    // And unlisting it takes it out again.
    upsert_model(
        &server,
        &model_name,
        serde_json::json!({ "isListed": false }),
    )
    .await;
    assert!(!public_model_ids(&server).await.contains(&model_name));
    assert_eq!(public_model_detail_status(&server, &model_name).await, 404);
    assert!(find_model(&internal_models(&server).await, &model_name).is_some());
}

#[tokio::test]
async fn usage_is_billed_against_an_unlisted_model() {
    let server = server_with_internal_token().await;
    let model_name = unique_model_name("unlisted-billing");
    upsert_model(&server, &model_name, new_model_fields(Some(false))).await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let workspace_id = list_workspaces(&server, org.id.clone())
        .await
        .first()
        .expect("org has a default workspace")
        .id
        .clone();
    let key =
        create_api_key_in_workspace(&server, workspace_id.clone(), "unlisted".to_string()).await;

    let response = server
        .post("/v1/internal/usage")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .json(&serde_json::json!({
            "organization_id": org.id,
            "workspace_id": workspace_id,
            "api_key_id": key.id,
            "type": "chat_completion",
            "model": model_name,
            "input_tokens": 1000,
            "output_tokens": 200,
            "id": format!("unlisted-usage-{}", uuid::Uuid::new_v4())
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    assert_eq!(body["model"], model_name);
    assert_eq!(body["input_cost"], 100_000i64);
    assert_eq!(body["output_cost"], 100_000i64);
    assert_eq!(body["total_cost"], 200_000i64);
}

#[tokio::test]
async fn internal_models_requires_the_service_token() {
    let server = server_with_internal_token().await;

    let missing = server.get("/v1/internal/models").await;
    assert_eq!(missing.status_code(), 401, "{}", missing.text());

    let wrong = server
        .get("/v1/internal/models")
        .add_header("Authorization", "Bearer not-the-token")
        .await;
    assert_eq!(wrong.status_code(), 401, "{}", wrong.text());

    // An API key is not a service token.
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let with_api_key = server
        .get("/v1/internal/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(with_api_key.status_code(), 401, "{}", with_api_key.text());
}

#[tokio::test]
async fn internal_models_is_disabled_without_a_configured_token() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/internal/models")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .await;
    assert_eq!(response.status_code(), 503, "{}", response.text());
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "endpoint_disabled");
}
