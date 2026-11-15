// E2E tests for the response headers that announce a planned model
// deprecation (`x-model-deprecation-date`, `x-model-successor`). The plan is
// written by the admin API; here it is seeded on the catalog row and the
// kept middleware is what is under test.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use chrono::{DateTime, Utc};

/// `2030-01-01` defaults to 13:00 UTC.
const DEPRECATION_DATE_HEADER: &str = "2030-01-01T13:00:00Z";

struct Fixture {
    server: axum_test::TestServer,
    api_key: String,
    old: String,
    old_alias: String,
    successor: String,
}

fn deprecation_date() -> DateTime<Utc> {
    DEPRECATION_DATE_HEADER.parse().unwrap()
}

fn model_upsert(aliases: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
        "modelDisplayName":   "Deprecation Headers Test Model",
        "modelDescription":   "Synthetic model for deprecation header e2e",
        "contextLength":      4096,
        "maxOutputLength":    1024,
        "verifiable":         false,
        "isActive":           true,
        "aliases":            aliases,
    })
}

async fn setup() -> Fixture {
    let (server, _router, inference_pool, mock_provider, _database) =
        setup_test_server_with_pool_and_router().await;
    let suffix = uuid::Uuid::new_v4();
    let old = format!("test-dep-headers-old-{suffix}");
    let old_alias = format!("test-dep-headers-alias-{suffix}");
    let successor = format!("test-dep-headers/Successor-{suffix}");

    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        old.clone(),
        serde_json::from_value(model_upsert(&[&old_alias])).unwrap(),
    );
    batch.insert(
        successor.clone(),
        serde_json::from_value(model_upsert(&[])).unwrap(),
    );
    seed_models(&server, batch, get_session_id()).await;

    let provider: std::sync::Arc<dyn inference_providers::InferenceProvider + Send + Sync> =
        mock_provider.clone();
    inference_pool
        .register_provider(old.clone(), provider)
        .await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    Fixture {
        server,
        api_key,
        old,
        old_alias,
        successor,
    }
}

async fn announce(fixture: &Fixture) {
    seeding::seed_planned_deprecation(
        &fixture.old,
        Some(deprecation_date()),
        Some(&fixture.successor),
    )
    .await;
}

async fn chat(fixture: &Fixture, model: &str, stream: bool) -> axum_test::TestResponse {
    let response = fixture
        .server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "Hello" }],
            "stream": stream,
            "max_tokens": 16
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response
}

fn header(response: &axum_test::TestResponse, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_string())
}

fn assert_announced(fixture: &Fixture, response: &axum_test::TestResponse, context: &str) {
    assert_eq!(
        header(response, "x-model-deprecation-date").as_deref(),
        Some(DEPRECATION_DATE_HEADER),
        "{context}"
    );
    assert_eq!(
        header(response, "x-model-successor"),
        Some(fixture.successor.clone()),
        "{context}"
    );
    // The endpoint is not being deprecated, so the URL-scoped standard
    // headers must not appear.
    for standard in ["deprecation", "sunset", "link"] {
        assert_eq!(header(response, standard), None, "{context}: {standard}");
    }
}

fn assert_not_announced(response: &axum_test::TestResponse) {
    assert_eq!(header(response, "x-model-deprecation-date"), None);
    assert_eq!(header(response, "x-model-successor"), None);
}

#[tokio::test]
async fn test_model_without_planned_deprecation_has_no_headers() {
    let fixture = setup().await;

    assert_not_announced(&chat(&fixture, &fixture.old, false).await);
    assert_not_announced(&chat(&fixture, &fixture.old, true).await);
}

#[tokio::test]
async fn test_planned_deprecation_is_announced_in_response_headers() {
    let fixture = setup().await;
    announce(&fixture).await;

    for stream in [false, true] {
        let response = chat(&fixture, &fixture.old, stream).await;
        assert_announced(&fixture, &response, &format!("stream={stream}"));
    }

    // The body is not annotated: only headers carry the announcement.
    let body: serde_json::Value = chat(&fixture, &fixture.old, false).await.json();
    assert!(body.get("warning").is_none(), "{body}");
}

#[tokio::test]
async fn test_deprecation_is_announced_on_a_non_chat_route() {
    let fixture = setup().await;
    announce(&fixture).await;

    let response = fixture
        .server
        .post("/v1/embeddings")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&serde_json::json!({ "model": fixture.old, "input": "Hello world" }))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_announced(&fixture, &response, "embeddings");
}

#[tokio::test]
async fn test_failed_request_is_not_annotated() {
    let fixture = setup().await;
    announce(&fixture).await;

    let response = fixture
        .server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .json(&serde_json::json!({ "model": fixture.old, "messages": [] }))
        .await;

    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert_not_announced(&response);
}

#[tokio::test]
async fn test_alias_request_gets_the_canonical_models_headers() {
    let fixture = setup().await;
    announce(&fixture).await;

    let response = chat(&fixture, &fixture.old_alias, false).await;

    assert_eq!(
        header(&response, "x-model-alias-resolved"),
        Some(format!("{} -> {}", fixture.old_alias, fixture.old))
    );
    assert_announced(&fixture, &response, "alias");
}

#[tokio::test]
async fn test_clearing_the_deprecation_date_removes_the_headers() {
    let fixture = setup().await;
    announce(&fixture).await;
    assert_announced(
        &fixture,
        &chat(&fixture, &fixture.old, false).await,
        "before clear",
    );

    // The successor goes with the date.
    seeding::seed_planned_deprecation(&fixture.old, None, None).await;

    assert_not_announced(&chat(&fixture, &fixture.old, false).await);
    assert_not_announced(&chat(&fixture, &fixture.old_alias, true).await);
}

#[tokio::test]
async fn test_clearing_only_the_successor_keeps_the_date() {
    let fixture = setup().await;
    announce(&fixture).await;

    seeding::seed_planned_deprecation(&fixture.old, Some(deprecation_date()), None).await;

    let response = chat(&fixture, &fixture.old, false).await;
    assert_eq!(
        header(&response, "x-model-deprecation-date").as_deref(),
        Some(DEPRECATION_DATE_HEADER)
    );
    assert_eq!(header(&response, "x-model-successor"), None);
}
