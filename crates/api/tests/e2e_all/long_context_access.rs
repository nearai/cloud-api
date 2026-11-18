//! End-to-end tests for per-organization access to a model's long-context
//! tier: a two-tier model (base 1_000 tokens, long 10_000) served by two
//! wiremock backends, and organizations that may or may not use the long
//! tier. Prompt estimate: ceil(text_bytes/4 × 1.2) + 4/message.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use database::repositories::PgLongContextAccessRepository;
use services::long_context_access::LongContextAccessRepository;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// About 6_000 tokens: above the base tier, inside the long one.
const OVERSIZED_BYTES: usize = 20_000;

fn completion_body(model: &str, tag: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("chatcmpl-{tag}-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": format!("served-by-{tag}")},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

async fn mount_models(server: &MockServer, model: &str) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": model, "object": "model", "owned_by": "nearai"}]
        })))
        .mount(server)
        .await;
}

async fn mount_tier(server: &MockServer, model: &str, tag: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_body(model, tag)))
        .mount(server)
        .await;
    mount_models(server, model).await;
}

async fn setup_tiered_model(
    server: &axum_test::TestServer,
    base_uri: &str,
    long_uri: &str,
) -> String {
    let model = format!("e2e-org/long-context-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000, "currency": "USD" },
            "modelDisplayName":   "Long-context access e2e",
            "modelDescription":   "Synthetic two-tier model",
            "contextLength":      10_000,
            "maxOutputLength":    1_024,
            "verifiable":         true,
            "isActive":           true,
            "providerType":       "vllm",
            "inferenceUrl":       base_uri,
            "providerConfig": {
                "long_context": {
                    "inference_url": long_uri,
                    "max_context_tokens": 10_000,
                    "base_max_context_tokens": 1_000
                }
            }
        }))
        .unwrap(),
    );
    let updated = seed_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "tiered model should upsert");
    model
}

async fn completions_hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count()
}

async fn chat(
    server: &axum_test::TestServer,
    api_key: &str,
    model: &str,
    content: String,
    stream: bool,
) -> axum_test::TestResponse {
    server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 10,
            "stream": stream
        }))
        .await
}

/// An organization with credits and a key. `disabled_models` is written to
/// its settings before it sends any request (the setting is cached per
/// organization once read).
async fn org_with_key(
    server: &axum_test::TestServer,
    database: &database::Database,
    disabled_models: Option<serde_json::Value>,
) -> (uuid::Uuid, String) {
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    let org_id: uuid::Uuid = org.id.parse().unwrap();
    if let Some(disabled_models) = disabled_models {
        database
            .pool()
            .get()
            .await
            .unwrap()
            .execute(
                "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), \
                 '{long_context_disabled_models}', $2::jsonb) WHERE id = $1",
                &[&org_id, &disabled_models],
            )
            .await
            .unwrap();
    }
    (org_id, get_api_key_for_org(server, org.id).await)
}

fn assert_context_length_exceeded(resp: &axum_test::TestResponse) {
    assert_eq!(resp.status_code(), 400, "body: {}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(
        body["error"]["message"],
        "This request needs more context than this model allows for your organization \
         (limit: 1000 tokens)."
    );
}

/// The organization's own switch: an oversized request is refused before any
/// backend sees it, a small one stays on the base tier, and another
/// organization still reaches the long tier.
#[tokio::test]
async fn e2e_org_switch_caps_requests_at_the_base_tier() {
    let (server, database) = setup_test_server_with_database().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base").await;
    mount_tier(&long, &model, "long").await;
    let (_, capped) = org_with_key(&server, &database, Some(serde_json::json!([model]))).await;
    let (_, open) = org_with_key(&server, &database, None).await;

    for stream in [false, true] {
        let resp = chat(
            &server,
            &capped,
            &model,
            "a".repeat(OVERSIZED_BYTES),
            stream,
        )
        .await;
        assert_context_length_exceeded(&resp);
    }
    // The Responses API runs on the same completion path.
    let resp = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {capped}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model,
            "input": "a".repeat(OVERSIZED_BYTES),
            "store": false,
            "stream": false
        }))
        .await;
    assert_context_length_exceeded(&resp);
    assert_eq!(completions_hits(&base).await, 0);
    assert_eq!(completions_hits(&long).await, 0);

    let resp = chat(&server, &capped, &model, "Hi".into(), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(resp.text().contains("served-by-base"), "{}", resp.text());

    let resp = chat(&server, &open, &model, "a".repeat(OVERSIZED_BYTES), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(resp.text().contains("served-by-long"), "{}", resp.text());
    assert_eq!(completions_hits(&long).await, 1);
}

/// A malformed stored setting turns the long tier off for every model.
#[tokio::test]
async fn e2e_malformed_org_switch_fails_closed() {
    let (server, database) = setup_test_server_with_database().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base").await;
    mount_tier(&long, &model, "long").await;
    let (_, capped) = org_with_key(&server, &database, Some(serde_json::json!("all"))).await;

    let resp = chat(&server, &capped, &model, "a".repeat(OVERSIZED_BYTES), false).await;
    assert_context_length_exceeded(&resp);
    assert_eq!(completions_hits(&long).await, 0);
}

/// The context-length-400 fall-through to the long tier is closed to a
/// capped organization: the base tier's 400 reaches the client.
#[tokio::test]
async fn e2e_org_switch_blocks_the_context_400_fall_through() {
    let (server, database) = setup_test_server_with_database().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "This model's maximum context length is 1000 tokens.",
                "type": "invalid_request_error"
            }
        })))
        .mount(&base)
        .await;
    mount_models(&base, &model).await;
    mount_tier(&long, &model, "long").await;
    let (_, capped) = org_with_key(&server, &database, Some(serde_json::json!([model]))).await;

    let resp = chat(&server, &capped, &model, "Hi".into(), false).await;
    assert_eq!(resp.status_code(), 400, "body: {}", resp.text());
    assert_eq!(completions_hits(&base).await, 1);
    assert_eq!(completions_hits(&long).await, 0);
}

async fn has_blocks_table(database: &database::Database) -> bool {
    database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT to_regclass('organization_model_long_context_blocks') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// The repository reads an organization's settings, and answers "no blocks"
/// (not an error) on a schema without the blocks table.
#[tokio::test]
async fn e2e_repository_reads_settings_and_tolerates_a_missing_blocks_table() {
    let (server, database) = setup_test_server_with_database().await;
    let repository = PgLongContextAccessRepository::new(database.pool().clone());
    let (org_id, _) = org_with_key(&server, &database, Some(serde_json::json!(["a/b"]))).await;

    let settings = repository
        .organization_settings(org_id)
        .await
        .unwrap()
        .expect("the organization has settings");
    assert_eq!(
        settings["long_context_disabled_models"],
        serde_json::json!(["a/b"])
    );
    assert_eq!(
        repository
            .organization_settings(uuid::Uuid::new_v4())
            .await
            .unwrap(),
        None
    );

    if !has_blocks_table(&database).await {
        assert_eq!(
            repository.blocked_model_names(org_id).await.unwrap(),
            Vec::<String>::new()
        );
    }
}

/// A platform block row caps the organization the same way. Runs only once
/// the database schema has the blocks table; earlier schemas skip it.
#[tokio::test]
async fn e2e_platform_block_caps_requests_at_the_base_tier() {
    let (server, database) = setup_test_server_with_database().await;
    let client = database.pool().get().await.unwrap();
    if !has_blocks_table(&database).await {
        eprintln!("skipped: the schema has no organization_model_long_context_blocks table");
        return;
    }

    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base").await;
    mount_tier(&long, &model, "long").await;
    let (blocked_org, blocked) = org_with_key(&server, &database, None).await;
    let (_, open) = org_with_key(&server, &database, None).await;
    client
        .execute(
            "INSERT INTO organization_model_long_context_blocks (organization_id, model_id) \
             SELECT $1, id FROM models WHERE model_name = $2",
            &[&blocked_org, &model],
        )
        .await
        .unwrap();
    assert_eq!(
        PgLongContextAccessRepository::new(database.pool().clone())
            .blocked_model_names(blocked_org)
            .await
            .unwrap(),
        vec![model.clone()]
    );

    let resp = chat(
        &server,
        &blocked,
        &model,
        "a".repeat(OVERSIZED_BYTES),
        false,
    )
    .await;
    assert_context_length_exceeded(&resp);
    let resp = chat(&server, &blocked, &model, "Hi".into(), false).await;
    assert!(resp.text().contains("served-by-base"), "{}", resp.text());
    assert_eq!(completions_hits(&long).await, 0);

    let resp = chat(&server, &open, &model, "a".repeat(OVERSIZED_BYTES), false).await;
    assert!(resp.text().contains("served-by-long"), "{}", resp.text());
}
