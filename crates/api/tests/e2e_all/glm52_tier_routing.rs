//! End-to-end tests for context-length tier routing (PR #860): a REAL request
//! enters the full app, flows through the REAL provider pool and REAL
//! `nearai::Provider` instances registered by the REAL admin-PATCH path, and
//! is served over the wire by one of two wiremock backends with different
//! declared context windows (base 1_000 tokens, long 10_000 tokens).
//!
//! This works without any attestation stack because the provider serves
//! non-rotation base URLs (IP literals like wiremock's `http://127.0.0.1:p`)
//! via its plain one-shot fallback client, and fingerprint blocking is
//! TLS-only.
//!
//! Routing arithmetic under test (see `refine_context_requirement`):
//!   required = ceil(text_bytes/4 × 1.2) + 4/message + max_tokens
//!   tokenize band = [0.7, 1.3] × capacity, on input estimate + reserve

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MAX_TOKENS: i64 = 10;

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

fn sse_body(model: &str, tag: &str) -> String {
    let chunk = serde_json::json!({
        "id": format!("chatcmpl-{tag}-stream"),
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": format!("served-by-{tag}")},
            "finish_reason": null
        }]
    });
    let done = serde_json::json!({
        "id": format!("chatcmpl-{tag}-stream"),
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n")
}

/// Mount the standard backend surface on a tier mock: completions (200,
/// tagged), the models list (discovery/catalog probes), and tokenize.
async fn mount_tier(server: &MockServer, model: &str, tag: &str, tokenize_count: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_body(model, tag)))
        .mount(server)
        .await;
    mount_models_and_tokenize(server, model, tokenize_count).await;
}

async fn mount_models_and_tokenize(server: &MockServer, model: &str, tokenize_count: u64) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": model, "object": "model", "owned_by": "nearai"}]
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/tokenize"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"count": tokenize_count})),
        )
        .mount(server)
        .await;
}

/// Register a two-tier model through the real admin PATCH path: base tier =
/// `base_uri` capped at 1_000 tokens, long tier = `long_uri` at 10_000 (also
/// the catalog contextLength — the customer-facing window).
async fn setup_tiered_model(
    server: &axum_test::TestServer,
    base_uri: &str,
    long_uri: &str,
) -> String {
    let model = format!("zai-e2e/glm52-tier-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000, "currency": "USD" },
            "modelDisplayName":   "GLM52 tier-routing e2e",
            "modelDescription":   "Synthetic two-tier model for tier-routing e2e",
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
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
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

async fn tokenize_hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/v1/tokenize")
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
            "max_tokens": MAX_TOKENS,
            "stream": stream
        }))
        .await
}

async fn org_key(server: &axum_test::TestServer) -> String {
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    get_api_key_for_org(server, org.id).await
}

/// Small request stays on the base fleet; the long tier is failover, not a
/// round-robin peer — it must see ZERO traffic.
#[tokio::test]
async fn e2e_small_request_served_by_base_tier() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 5).await;
    mount_tier(&long, &model, "long", 5).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "Hi".into(), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-base"),
        "small request must be served by the base tier, got: {}",
        resp.text()
    );
    assert_eq!(
        completions_hits(&long).await,
        0,
        "the long tier must see zero traffic for small requests"
    );
}

/// A request that cannot fit the base window (countable ≈ 5_000 > 1_000)
/// routes to the long tier — the core of the feature.
#[tokio::test]
async fn e2e_oversize_request_served_by_long_tier() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 5).await;
    mount_tier(&long, &model, "long", 5).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "a".repeat(20_000), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-long"),
        "oversize request must be served by the long tier, got: {}",
        resp.text()
    );
    assert_eq!(
        completions_hits(&base).await,
        0,
        "the base tier must not be asked to serve an oversize request"
    );
}

/// Near the boundary ([0.7, 1.3]×cap) the pool asks the backend for an EXACT
/// count and it overrides the heuristic: a 4_000-byte prompt (heuristic ≈
/// 1_214 > cap) with a real count of 100 stays on base.
#[tokio::test]
async fn e2e_exact_tokenize_count_overrides_heuristic() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 100).await;
    mount_tier(&long, &model, "long", 100).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "a".repeat(4_000), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-base"),
        "exact count 100 must keep the request on base despite the ~1_214 heuristic, got: {}",
        resp.text()
    );
    assert_eq!(
        tokenize_hits(&base).await,
        1,
        "the boundary-band request must trigger exactly one tokenize call on the base fleet"
    );
    assert_eq!(
        tokenize_hits(&long).await,
        0,
        "tokenize must run on the base fleet, not the long host"
    );
}

/// Counter-case: same boundary prompt, but the exact count (2_000) confirms
/// the request does NOT fit base → long serves.
#[tokio::test]
async fn e2e_exact_tokenize_count_confirms_long_tier() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 2_000).await;
    mount_tier(&long, &model, "long", 2_000).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "a".repeat(4_000), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-long"),
        "exact count 2_000 must send the boundary request to the long tier, got: {}",
        resp.text()
    );
    assert_eq!(tokenize_hits(&base).await, 1);
}

/// Defense-in-depth: if an under-estimated request reaches the base fleet and
/// gets the engine's context-length 400, it falls through to the long tier
/// instead of failing the client.
#[tokio::test]
async fn e2e_context_length_400_falls_through_to_long_tier() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    // Base REJECTS with the real engine phrasing instead of serving.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "This model's maximum context length is 1000 tokens. \
                            However, you requested 1500 tokens. Please reduce the length.",
                "type": "invalid_request_error"
            }
        })))
        .mount(&base)
        .await;
    mount_models_and_tokenize(&base, &model, 5).await;
    mount_tier(&long, &model, "long", 5).await;
    let api_key = org_key(&server).await;

    // Small prompt → routed to base first → 400 → must fall through to long.
    let resp = chat(&server, &api_key, &model, "Hi".into(), false).await;
    assert_eq!(
        resp.status_code(),
        200,
        "context-400 must fall through to the bigger tier, body: {}",
        resp.text()
    );
    assert!(
        resp.text().contains("served-by-long"),
        "got: {}",
        resp.text()
    );
    assert_eq!(completions_hits(&base).await, 1, "base tried once");
}

/// Regression for the review fix: when the long tier is saturated (503) and
/// the base fleet then rejects with a context-400, the client must see the
/// RETRYABLE 5xx — not a misleading "maximum context length" 400 that would
/// stop it from retrying a servable request. (~3.5s: retry rounds.)
#[tokio::test]
async fn e2e_saturated_long_tier_surfaces_retryable_error_not_context_400() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "This model's maximum context length is 1000 tokens."}
        })))
        .mount(&base)
        .await;
    mount_models_and_tokenize(&base, &model, 5_000).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {"message": "queue full"}
        })))
        .mount(&long)
        .await;
    mount_models_and_tokenize(&long, &model, 5_000).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "a".repeat(20_000), false).await;
    assert_ne!(
        resp.status_code(),
        400,
        "the base tier's expected context-400 must not mask the long tier's 503, body: {}",
        resp.text()
    );
    // cloud-api maps the provider 503 to 429 service_overloaded ("please
    // retry with exponential backoff") — the point is the client gets a
    // RETRYABLE signal for a servable request, never a permanent-looking
    // context-length 400.
    assert!(
        resp.status_code() == 429 || resp.status_code().as_u16() >= 500,
        "terminal error must be retryable-class (429/5xx), got {} body: {}",
        resp.status_code(),
        resp.text()
    );
    assert!(
        resp.text().contains("overloaded") || resp.status_code().as_u16() >= 500,
        "client-facing error should say overloaded/retry, body: {}",
        resp.text()
    );
}

/// Streaming takes the same tier decision (covers chat_completion_stream's
/// fallback-client path and the first-chunk peek).
#[tokio::test]
async fn e2e_streaming_oversize_request_served_by_long_tier() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 5).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body(&model, "long"), "text/event-stream"),
        )
        .mount(&long)
        .await;
    mount_models_and_tokenize(&long, &model, 5).await;
    let api_key = org_key(&server).await;

    let resp = chat(&server, &api_key, &model, "a".repeat(20_000), true).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-long"),
        "streamed oversize request must be served by the long tier, got: {}",
        resp.text()
    );
    assert_eq!(completions_hits(&base).await, 0);
}

/// Transparency guarantee: the tiering is invisible in the public catalog —
/// ONE entry for the model, advertising the full (long-tier) window.
#[tokio::test]
async fn e2e_catalog_shows_single_entry_with_full_context() {
    let server = setup_test_server().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 5).await;
    mount_tier(&long, &model, "long", 5).await;

    let resp = server.get("/v1/models").await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    let entries: Vec<&serde_json::Value> = body["data"]
        .as_array()
        .expect("models list")
        .iter()
        .filter(|m| m["id"] == serde_json::Value::String(model.clone()))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "the two-tier model must appear exactly once in the public catalog"
    );
    assert_eq!(
        entries[0]["context_length"]
            .as_i64()
            .or(entries[0]["contextLength"].as_i64()),
        Some(10_000),
        "catalog must advertise the full long-tier window, entry: {}",
        entries[0]
    );
}

/// The full fallback chain: long tier saturated → the pinned attested
/// fallback (Chutes stand-in at the pool boundary — its wire client does
/// ML-KEM E2EE + TDX verification and cannot be HTTP-mocked by design)
/// serves the oversize request via the real tier ordering + retry chain.
#[tokio::test]
async fn e2e_saturated_long_tier_falls_back_to_pinned_attested_provider() {
    use inference_providers::mock::{MockProvider, RequestMatcher, ResponseTemplate as MockRt};
    use inference_providers::ProviderTier;
    use std::sync::Arc;

    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    let (base, long) = (MockServer::start().await, MockServer::start().await);
    let model = setup_tiered_model(&server, &base.uri(), &long.uri()).await;
    mount_tier(&base, &model, "base", 5_000).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {"message": "queue full"}
        })))
        .mount(&long)
        .await;
    mount_models_and_tokenize(&long, &model, 5_000).await;

    let chutes = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Attested3p));
    chutes
        .when(RequestMatcher::Any)
        .respond_with(MockRt::new("served-by-chutes-fallback"))
        .await;
    pool.register_pinned_secondary_provider(model.clone(), chutes, Some(10_000))
        .await;

    let api_key = org_key(&server).await;
    let resp = chat(&server, &api_key, &model, "a".repeat(20_000), false).await;
    assert_eq!(resp.status_code(), 200, "body: {}", resp.text());
    assert!(
        resp.text().contains("served-by-chutes-fallback"),
        "saturated long tier must fall back to the pinned attested provider, got: {}",
        resp.text()
    );
    assert_eq!(
        completions_hits(&base).await,
        0,
        "the too-small base fleet is the LAST resort, not part of the chain here"
    );
}
