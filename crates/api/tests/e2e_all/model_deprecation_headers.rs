// E2E tests for the response headers that announce a planned model
// deprecation: `Deprecation` (RFC 9745), `Sunset` (RFC 8594) and
// `Link: rel="successor-version"`, plus the catalog fields behind them.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;

/// `2030-01-01` defaults to 13:00 UTC; that day is a Tuesday.
const DEPRECATION_DATE: &str = "2030-01-01";
const SUNSET_HEADER: &str = "Tue, 01 Jan 2030 13:00:00 GMT";

struct Fixture {
    server: axum_test::TestServer,
    api_key: String,
    /// Model that gets a planned deprecation. No `/` in the name: the confirm
    /// endpoint takes it as a path segment.
    old: String,
    old_alias: String,
    /// Contains a `/`, so the `Link` target must percent-encode it.
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

/// The `Deprecation` header as unix seconds.
fn deprecation_timestamp(response: &axum_test::TestResponse) -> i64 {
    let value = header(response, "deprecation").expect("response must carry Deprecation");
    value
        .strip_prefix('@')
        .unwrap_or_else(|| panic!("Deprecation must be a structured-field date, got {value}"))
        .parse()
        .unwrap_or_else(|_| panic!("Deprecation must be @<unix seconds>, got {value}"))
}

fn assert_no_deprecation_headers(response: &axum_test::TestResponse) {
    assert_eq!(header(response, "deprecation"), None);
    assert_eq!(header(response, "sunset"), None);
    assert_eq!(header(response, "link"), None);
}

fn successor_link(fixture: &Fixture) -> String {
    format!(
        "</v1/model/{}>; rel=\"successor-version\"",
        fixture.successor.replace('/', "%2F")
    )
}

#[tokio::test]
async fn test_model_without_planned_deprecation_has_no_headers() {
    let fixture = setup().await;

    assert_no_deprecation_headers(&chat(&fixture, &fixture.old, false).await);
    assert_no_deprecation_headers(&chat(&fixture, &fixture.old, true).await);
}

#[tokio::test]
async fn test_confirmed_deprecation_is_announced_in_response_headers() {
    let fixture = setup().await;
    let before = chrono::Utc::now().timestamp();
    confirm_deprecation(&fixture).await;
    let after = chrono::Utc::now().timestamp();

    for stream in [false, true] {
        let response = chat(&fixture, &fixture.old, stream).await;

        let announced = deprecation_timestamp(&response);
        assert!(
            (before - 1..=after + 1).contains(&announced),
            "stream={stream}: Deprecation {announced} should be the confirm time ({before}..={after})"
        );
        assert_eq!(
            header(&response, "sunset").as_deref(),
            Some(SUNSET_HEADER),
            "stream={stream}"
        );
        assert_eq!(
            header(&response, "link"),
            Some(successor_link(&fixture)),
            "stream={stream}"
        );
    }

    // The body is not annotated: only headers carry the announcement.
    let body: serde_json::Value = chat(&fixture, &fixture.old, false).await.json();
    assert!(body.get("warning").is_none(), "{body}");
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
    deprecation_timestamp(&response);
    assert_eq!(header(&response, "sunset").as_deref(), Some(SUNSET_HEADER));
    assert_eq!(header(&response, "link"), Some(successor_link(&fixture)));
}

#[tokio::test]
async fn test_clearing_the_deprecation_date_removes_the_headers() {
    let fixture = setup().await;
    confirm_deprecation(&fixture).await;
    assert!(header(&chat(&fixture, &fixture.old, false).await, "sunset").is_some());

    let cleared = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "deprecationDate": null } }),
    )
    .await;
    assert_eq!(cleared.status_code(), 200, "{}", cleared.text());
    let cleared: serde_json::Value = cleared.json();
    let metadata = &cleared[0]["metadata"];
    assert!(metadata.get("deprecationDate").is_none(), "{metadata}");
    assert!(
        metadata.get("deprecationAnnouncedAt").is_none(),
        "{metadata}"
    );
    assert!(metadata.get("successorModelId").is_none(), "{metadata}");

    assert_no_deprecation_headers(&chat(&fixture, &fixture.old, false).await);
}

#[tokio::test]
async fn test_patch_stamps_the_announcement_once_and_exposes_the_successor() {
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
    assert_eq!(metadata["deprecationDate"], "2030-01-01T13:00:00Z");
    assert_eq!(metadata["successorModelId"], fixture.successor.as_str());
    let announced_at = metadata["deprecationAnnouncedAt"]
        .as_str()
        .expect("announcement time must be stamped")
        .to_string();

    // Moving the date keeps the original announcement.
    let moved = patch_models(
        &fixture.server,
        serde_json::json!({ &fixture.old: { "deprecationDate": "2031-06-01" } }),
    )
    .await;
    assert_eq!(moved.status_code(), 200, "{}", moved.text());
    let moved: serde_json::Value = moved.json();
    let metadata = &moved[0]["metadata"];
    assert_eq!(metadata["deprecationDate"], "2031-06-01T13:00:00Z");
    assert_eq!(metadata["deprecationAnnouncedAt"], announced_at.as_str());
    assert_eq!(metadata["successorModelId"], fixture.successor.as_str());

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

    // The audit trail records both fields.
    let history = fixture
        .server
        .get(&format!("/v1/admin/models/{}/history", fixture.old))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(history.status_code(), 200, "{}", history.text());
    let history: serde_json::Value = history.json();
    let latest = &history["history"][0];
    assert_eq!(latest["deprecationAnnouncedAt"], announced_at.as_str());
    assert_eq!(latest["successorModelId"], fixture.successor.as_str());
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
    assert_no_deprecation_headers(&chat(&fixture, &fixture.old, false).await);
}
