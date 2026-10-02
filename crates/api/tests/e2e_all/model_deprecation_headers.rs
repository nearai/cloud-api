// E2E tests for the response headers that announce a planned model
// deprecation (`x-model-deprecation-date`, `x-model-successor`) and the
// catalog field behind the successor.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

/// `2030-01-01` defaults to 13:00 UTC.
const DEPRECATION_DATE: &str = "2030-01-01";
const DEPRECATION_DATE_HEADER: &str = "2030-01-01T13:00:00Z";

struct Fixture {
    server: axum_test::TestServer,
    api_key: String,
    /// Model that gets a planned deprecation. No `/` in the name: the confirm
    /// endpoint takes it as a path segment.
    old: String,
    old_alias: String,
    successor: String,
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
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

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

async fn patch_models(
    server: &axum_test::TestServer,
    body: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn confirm_deprecation(fixture: &Fixture) {
    let response = fixture
        .server
        .post(&format!(
            "/v1/admin/models/{}/deprecation/confirm",
            fixture.old
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "successorModelId": fixture.successor,
            "deprecationDate": DEPRECATION_DATE,
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
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
async fn test_confirmed_deprecation_is_announced_in_response_headers() {
    let fixture = setup().await;
    confirm_deprecation(&fixture).await;

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
    confirm_deprecation(&fixture).await;

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
    confirm_deprecation(&fixture).await;

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
    confirm_deprecation(&fixture).await;

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
    confirm_deprecation(&fixture).await;
    assert_announced(
        &fixture,
        &chat(&fixture, &fixture.old, false).await,
        "before clear",
    );

    let cleared = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "deprecationDate": null } }),
    )
    .await;
    assert_eq!(cleared.status_code(), 200, "{}", cleared.text());
    let cleared: serde_json::Value = cleared.json();
    let metadata = &cleared[0]["metadata"];
    assert!(metadata.get("deprecationDate").is_none(), "{metadata}");
    // The successor goes with the date.
    assert!(metadata.get("successorModelId").is_none(), "{metadata}");

    assert_not_announced(&chat(&fixture, &fixture.old, false).await);
}

#[tokio::test]
async fn test_patch_sets_the_successor_and_the_catalog_exposes_it() {
    let fixture = setup().await;

    let set = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: {
            "deprecationDate": DEPRECATION_DATE,
            "successorModelId": fixture.successor,
        } }),
    )
    .await;
    assert_eq!(set.status_code(), 200, "{}", set.text());
    let set: serde_json::Value = set.json();
    let metadata = &set[0]["metadata"];
    assert_eq!(metadata["deprecationDate"], DEPRECATION_DATE_HEADER);
    assert_eq!(metadata["successorModelId"], fixture.successor.as_str());

    // Moving the date keeps the successor; the header follows the new date.
    let moved = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "deprecationDate": "2031-06-01" } }),
    )
    .await;
    assert_eq!(moved.status_code(), 200, "{}", moved.text());
    let moved: serde_json::Value = moved.json();
    let metadata = &moved[0]["metadata"];
    assert_eq!(metadata["deprecationDate"], "2031-06-01T13:00:00Z");
    assert_eq!(metadata["successorModelId"], fixture.successor.as_str());

    let response = chat(&fixture, &fixture.old, false).await;
    assert_eq!(
        header(&response, "x-model-deprecation-date").as_deref(),
        Some("2031-06-01T13:00:00Z")
    );
    assert_eq!(
        header(&response, "x-model-successor"),
        Some(fixture.successor.clone())
    );

    // The public catalog names the successor next to the date.
    let models = fixture
        .server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", fixture.api_key))
        .await;
    assert_eq!(models.status_code(), 200, "{}", models.text());
    let models: serde_json::Value = models.json();
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == fixture.old.as_str())
        .expect("model must be listed");
    assert_eq!(entry["deprecation_date"], "2031-06-01T13:00:00Z");
    assert_eq!(entry["successor_model_id"], fixture.successor.as_str());

    // The audit trail records the successor.
    let history = fixture
        .server
        .get(&format!("/v1/admin/models/{}/history", fixture.old))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(history.status_code(), 200, "{}", history.text());
    let history: serde_json::Value = history.json();
    assert_eq!(
        history["history"][0]["successorModelId"],
        fixture.successor.as_str()
    );
}

#[tokio::test]
async fn test_patch_rejects_an_invalid_successor() {
    let fixture = setup().await;

    let unknown = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: {
            "deprecationDate": DEPRECATION_DATE,
            "successorModelId": format!("test-dep-headers/missing-{}", uuid::Uuid::new_v4()),
        } }),
    )
    .await;
    assert_eq!(unknown.status_code(), 400, "{}", unknown.text());

    let itself = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: {
            "deprecationDate": DEPRECATION_DATE,
            "successorModelId": fixture.old,
        } }),
    )
    .await;
    assert_eq!(itself.status_code(), 400, "{}", itself.text());

    // Nothing was written by the rejected requests.
    assert_not_announced(&chat(&fixture, &fixture.old, false).await);
}

#[tokio::test]
async fn test_patch_rejects_a_successor_without_a_planned_date() {
    let fixture = setup().await;

    // The model has no planned date and the request does not set one.
    let no_date = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "successorModelId": fixture.successor } }),
    )
    .await;
    assert_eq!(no_date.status_code(), 400, "{}", no_date.text());

    // The request clears the date it would need.
    confirm_deprecation(&fixture).await;
    let cleared = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: {
            "deprecationDate": null,
            "successorModelId": fixture.successor,
        } }),
    )
    .await;
    assert_eq!(cleared.status_code(), 400, "{}", cleared.text());

    // The rejected request left the confirmed deprecation in place.
    assert_announced(
        &fixture,
        &chat(&fixture, &fixture.old, false).await,
        "after rejected clear",
    );
}

#[tokio::test]
async fn test_clearing_only_the_successor_keeps_the_date() {
    let fixture = setup().await;
    confirm_deprecation(&fixture).await;

    let cleared = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "successorModelId": null } }),
    )
    .await;
    assert_eq!(cleared.status_code(), 200, "{}", cleared.text());

    let response = chat(&fixture, &fixture.old, false).await;
    assert_eq!(
        header(&response, "x-model-deprecation-date").as_deref(),
        Some(DEPRECATION_DATE_HEADER)
    );
    assert_eq!(header(&response, "x-model-successor"), None);
}

#[tokio::test]
async fn test_same_request_successor_must_end_up_active() {
    let fixture = setup().await;

    // The same request deactivates the successor.
    let deactivated = patch_models(
        &fixture.server,
        serde_json::json!({
            &fixture.old: {
                "deprecationDate": DEPRECATION_DATE,
                "successorModelId": fixture.successor,
            },
            &fixture.successor: { "isActive": false },
        }),
    )
    .await;
    assert_eq!(deactivated.status_code(), 400, "{}", deactivated.text());

    // The successor is already inactive and the request leaves that alone.
    let deactivate = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.successor: { "isActive": false } }),
    )
    .await;
    assert_eq!(deactivate.status_code(), 200, "{}", deactivate.text());
    let inactive = patch_models(
        &fixture.server,
        serde_json::json!({
            &fixture.old: {
                "deprecationDate": DEPRECATION_DATE,
                "successorModelId": fixture.successor,
            },
            &fixture.successor: { "modelDescription": "Still inactive" },
        }),
    )
    .await;
    assert_eq!(inactive.status_code(), 400, "{}", inactive.text());
    assert_not_announced(&chat(&fixture, &fixture.old, false).await);

    // A successor created by the same request is active by default.
    let created = format!("test-dep-headers/Created-{}", uuid::Uuid::new_v4());
    let accepted = patch_models(
        &fixture.server,
        serde_json::json!({
            &fixture.old: {
                "deprecationDate": DEPRECATION_DATE,
                "successorModelId": created,
            },
            &created: model_upsert(&[]),
        }),
    )
    .await;
    assert_eq!(accepted.status_code(), 200, "{}", accepted.text());
    let response = chat(&fixture, &fixture.old, false).await;
    assert_eq!(header(&response, "x-model-successor"), Some(created));
}
