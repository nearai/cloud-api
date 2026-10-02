use crate::common::*;

/// Shared service token used by every test in this module to authenticate
/// against `POST /v1/internal/usage`.
const INTERNAL_USAGE_TOKEN: &str = "test-internal-secret";

/// Identity triple (plus the raw `sk-…` secret) for a provisioned org +
/// workspace + API key. The internal usage endpoint trusts these as-is in
/// the request body; `api_key` is only needed by tests that also drive the
/// inference pipeline directly.
struct UsageIdentity {
    org_id: String,
    workspace_id: String,
    api_key_id: String,
    api_key: String,
}

/// Spin up a test server with the internal usage endpoint enabled. The
/// legacy `sk-…`-authenticated `POST /v1/usage` has been removed; usage is
/// recorded exclusively through the service-token `/v1/internal/usage`
/// path, which shares the same `record_usage_from_api` service logic.
async fn enable_internal_usage_server() -> axum_test::TestServer {
    setup_test_server_with_config(|c| {
        c.internal_usage_token = Some(INTERNAL_USAGE_TOKEN.to_string());
    })
    .await
}

/// Provision an org with $10 credits, grab its default workspace, and mint
/// an API key — returning the identity the internal usage body requires.
async fn provision_identity(server: &axum_test::TestServer) -> UsageIdentity {
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    let workspaces = list_workspaces(server, org.id.clone()).await;
    let workspace_id = workspaces
        .first()
        .expect("org should have a default workspace")
        .id
        .clone();
    let key =
        create_api_key_in_workspace(server, workspace_id.clone(), "internal-usage".to_string())
            .await;
    UsageIdentity {
        org_id: org.id,
        workspace_id,
        api_key_id: key.id,
        api_key: key.key.expect("freshly created API key returns its secret"),
    }
}

/// Merge the identity triple into a usage payload to form the
/// `/v1/internal/usage` request body.
fn internal_usage_body(id: &UsageIdentity, mut usage: serde_json::Value) -> serde_json::Value {
    let obj = usage
        .as_object_mut()
        .expect("usage payload must be a JSON object");
    obj.insert("organization_id".into(), serde_json::json!(id.org_id));
    obj.insert("workspace_id".into(), serde_json::json!(id.workspace_id));
    obj.insert("api_key_id".into(), serde_json::json!(id.api_key_id));
    usage
}

/// POST a usage payload to `/v1/internal/usage` with the shared service token.
async fn post_internal_usage(
    server: &axum_test::TestServer,
    id: &UsageIdentity,
    usage: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post("/v1/internal/usage")
        .add_header("Authorization", format!("Bearer {INTERNAL_USAGE_TOKEN}"))
        .json(&internal_usage_body(id, usage))
        .await
}

/// Happy-path test for `POST /v1/internal/usage` with type=chat_completion.
/// Sets up a model with pricing, creates an org with credits, and records usage.
#[tokio::test]
async fn test_record_chat_completion_usage() {
    let server = enable_internal_usage_server().await;

    // Setup model with known pricing (input: 1_000_000, output: 2_000_000 nano-dollars per token)
    setup_qwen_model(&server).await;

    // Setup org with $10 credits and an API key identity
    let id = provision_identity(&server).await;

    // Record chat completion usage
    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-chat-completion-001"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();

    // Verify tagged union type
    assert_eq!(body["type"], "chat_completion");

    // Verify token counts
    assert_eq!(body["input_tokens"], 100);
    assert_eq!(body["output_tokens"], 50);
    assert_eq!(body["total_tokens"], 150);
    // cache_read_tokens is not provided, should default to 0 (no cache hits)
    assert_eq!(body["cache_read_tokens"], 0);

    // Verify costs are calculated correctly
    // input: 100 tokens * 1_000_000 nano-dollars = 100_000_000
    // output: 50 tokens * 2_000_000 nano-dollars = 100_000_000
    // total: 200_000_000
    assert_eq!(body["input_cost"], 100_000_000i64);
    assert_eq!(body["output_cost"], 100_000_000i64);
    assert_eq!(body["total_cost"], 200_000_000i64);

    // Verify model name
    assert_eq!(body["model"], "Qwen/Qwen3-30B-A3B-Instruct-2507");

    // Verify id and created_at are present
    assert!(body["id"].is_string(), "id should be present");
    assert!(
        body["created_at"].is_string(),
        "created_at should be present"
    );

    // Verify total_cost_display is human-readable
    assert!(
        body["total_cost_display"].is_string(),
        "total_cost_display should be present"
    );
}

/// Happy-path test for `POST /v1/internal/usage` with type=image_generation.
#[tokio::test]
async fn test_record_image_generation_usage() {
    let server = enable_internal_usage_server().await;

    // Setup image model with cost_per_image pricing (40_000_000 nano-dollars per image)
    setup_qwen_image_model(&server).await;

    // Setup org with $10 credits and an API key identity
    let id = provision_identity(&server).await;

    // Record image generation usage
    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "image_generation",
            "model": "Qwen/Qwen-Image-2512",
            "image_count": 3,
            "id": "test-image-gen-001"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();

    // Verify tagged union type
    assert_eq!(body["type"], "image_generation");

    // Verify image count
    assert_eq!(body["image_count"], 3);

    // Verify cost: 3 images * 40_000_000 nano-dollars = 120_000_000
    assert_eq!(body["total_cost"], 120_000_000i64);

    // Verify model name
    assert_eq!(body["model"], "Qwen/Qwen-Image-2512");

    // Verify id and created_at are present
    assert!(body["id"].is_string(), "id should be present");
    assert!(
        body["created_at"].is_string(),
        "created_at should be present"
    );

    // Image generation response should NOT contain token fields
    assert!(
        body.get("input_tokens").is_none(),
        "input_tokens should not be in image_generation response"
    );
    assert!(
        body.get("output_tokens").is_none(),
        "output_tokens should not be in image_generation response"
    );
}

/// Test that the required `id` field is stored and does not affect the response shape.
#[tokio::test]
async fn test_record_usage_with_external_id() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 10,
            "output_tokens": 20,
            "id": "chatcmpl-ext-12345"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording with external id should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["type"], "chat_completion");
    assert_eq!(body["input_tokens"], 10);
    assert_eq!(body["output_tokens"], 20);
    assert_eq!(body["cache_read_tokens"], 0);

    // The response id should be the usage log row's primary key (not the external id)
    assert!(body["id"].is_string());
}

/// Test validation: model not found returns 404.
#[tokio::test]
async fn test_record_usage_model_not_found() {
    let server = enable_internal_usage_server().await;

    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "nonexistent/model",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-not-found-001"
        }),
    )
    .await;

    assert_eq!(response.status_code(), 404);
}

/// Test validation: zero tokens returns 400.
#[tokio::test]
async fn test_record_usage_zero_tokens() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 0,
            "output_tokens": 0,
            "id": "test-zero-tokens-001"
        }),
    )
    .await;

    assert_eq!(response.status_code(), 400);
}

/// Test validation: missing `id` field returns 400 (deserialization error).
///
/// Unlike the removed `POST /v1/usage` (which used the `Json` extractor and
/// returned 422 on a bad body), `/v1/internal/usage` deserializes raw bytes
/// itself and maps any parse failure to 400/`validation_error`.
#[tokio::test]
async fn test_record_usage_missing_id_field() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50
        }),
    )
    .await;

    // Missing required `id` field should fail deserialization
    assert_eq!(
        response.status_code(),
        400,
        "Missing id should return 400: {}",
        response.text()
    );
}

/// Test validation: empty `id` field returns 400.
#[tokio::test]
async fn test_record_usage_empty_id() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": ""
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        400,
        "Empty id should return 400: {}",
        response.text()
    );
}

/// Idempotency test: calling `POST /v1/internal/usage` twice with the same
/// `id` returns the same record both times and only charges the organization
/// once.
#[tokio::test]
async fn test_record_usage_idempotent_duplicate() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let payload = serde_json::json!({
        "type": "chat_completion",
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
        "input_tokens": 100,
        "output_tokens": 50,
        "id": "idempotency-test-same-id"
    });

    // First call — creates the record
    let response1 = post_internal_usage(&server, &id, payload.clone()).await;

    assert_eq!(
        response1.status_code(),
        200,
        "First call should succeed: {}",
        response1.text()
    );
    let body1: serde_json::Value = response1.json();

    // Second call — should return existing record (no double-charge)
    let response2 = post_internal_usage(&server, &id, payload).await;

    assert_eq!(
        response2.status_code(),
        200,
        "Duplicate call should also succeed: {}",
        response2.text()
    );
    let body2: serde_json::Value = response2.json();

    // Both responses should return the same usage record (same primary key id)
    assert_eq!(
        body1["id"], body2["id"],
        "Both calls should return the same record id"
    );
    assert_eq!(body1["total_cost"], body2["total_cost"]);
    assert_eq!(body1["created_at"], body2["created_at"]);

    // Verify the balance was only charged once (total_cost = 200_000_000)
    // by recording a second distinct usage and checking the combined balance
    let response3 = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 10,
            "output_tokens": 5,
            "id": "idempotency-test-different-id"
        }),
    )
    .await;

    assert_eq!(response3.status_code(), 200);
}

/// Test that two different organizations can use the same external `id`
/// without conflicting — the idempotency scope is per-organization.
#[tokio::test]
async fn test_record_usage_same_id_different_orgs() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;

    // Setup two separate org identities on the same server
    let id1 = provision_identity(&server).await;
    let id2 = provision_identity(&server).await;

    let shared_id = "shared-external-id-across-orgs";

    // Org1 records usage with the shared id
    let response1 = post_internal_usage(
        &server,
        &id1,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": shared_id
        }),
    )
    .await;

    assert_eq!(
        response1.status_code(),
        200,
        "Org1 usage should succeed: {}",
        response1.text()
    );

    // Org2 records usage with the same id — should NOT conflict
    let response2 = post_internal_usage(
        &server,
        &id2,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 200,
            "output_tokens": 100,
            "id": shared_id
        }),
    )
    .await;

    assert_eq!(
        response2.status_code(),
        200,
        "Org2 usage with same id should succeed: {}",
        response2.text()
    );

    let body1: serde_json::Value = response1.json();
    let body2: serde_json::Value = response2.json();

    // The two records should be different (different primary key ids)
    assert_ne!(
        body1["id"], body2["id"],
        "Different orgs should create separate records even with same external id"
    );

    // Costs should reflect the different token counts
    assert_eq!(body1["input_tokens"], 100);
    assert_eq!(body2["input_tokens"], 200);
}

/// Test recording usage with cache_read_tokens and cache-read pricing enabled.
/// Verifies that cache hits reduce input cost according to cache_read_cost_per_token.
#[tokio::test]
async fn test_record_chat_completion_usage_with_cached_tokens() {
    let server = enable_internal_usage_server().await;

    // Setup model with cache-read pricing:
    // input: 1_000_000, output: 2_000_000, cache_read: 500_000 nano-dollars per token
    let model = setup_qwen_model_with_cache_pricing(&server).await;

    // Setup org with $10 credits and an API key identity
    let id = provision_identity(&server).await;

    // Record chat completion usage with 40 cached prompt tokens out of 100
    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": model,
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_tokens": 40,
            "id": "test-chat-completion-with-cache"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording with cache_read_tokens should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();

    // Verify tagged union type
    assert_eq!(body["type"], "chat_completion");

    // Verify token counts
    assert_eq!(body["input_tokens"], 100);
    assert_eq!(body["output_tokens"], 50);
    assert_eq!(body["total_tokens"], 150);
    assert_eq!(body["cache_read_tokens"], 40);

    // Cost expectations with cache pricing:
    // - input_tokens = 100, cache_read_tokens = 40
    // - non_cached_input = 60
    // - input_cost = 60 * 1_000_000 + 40 * 500_000 = 80_000_000
    // - output_cost = 50 * 2_000_000 = 100_000_000
    // - total_cost = 180_000_000
    assert_eq!(body["input_cost"], 80_000_000i64);
    assert_eq!(body["output_cost"], 100_000_000i64);
    assert_eq!(body["total_cost"], 180_000_000i64);
}

/// Pins the invariant that an external usage submission (`/v1/internal/usage`)
/// keyed by a provider-style id (`chatcmpl-…`) does not share an
/// `inference_id` with the internal chat-completion pipeline's record for the
/// same provider id. Both writes must land independently in usage history;
/// the per-org idempotency constraint on `inference_id` applies only within
/// each source.
#[tokio::test]
async fn test_external_usage_record_does_not_collide_with_internal_pipeline() {
    use inference_providers::StreamChunk;

    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    // Drive a streaming completion so we can capture the provider-assigned
    // chat id from the SSE chunks (same id the internal pipeline will key
    // its billing record under after stream finalize).
    let stream_resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", id.api_key))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true
        }))
        .await;
    assert_eq!(stream_resp.status_code(), 200);

    let body = stream_resp.text();
    let provider_id = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| data.trim() != "[DONE]")
        .find_map(|data| match serde_json::from_str::<StreamChunk>(data) {
            Ok(StreamChunk::Chat(c)) => Some(c.id),
            _ => None,
        })
        .expect("stream should contain at least one chat chunk with an id");
    assert!(
        provider_id.starts_with("chatcmpl-"),
        "mock provider should emit chatcmpl-style ids, got {provider_id:?}",
    );

    // Submit external usage carrying the same provider id. The namespace
    // prefix on external ids must keep this from sharing an inference_id
    // slot with the internal pipeline's record.
    let external_resp = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 1,
            "output_tokens": 1,
            "id": provider_id,
        }),
    )
    .await;
    assert_eq!(
        external_resp.status_code(),
        200,
        "external usage submission should succeed: {}",
        external_resp.text()
    );

    // Poll usage history until both records land. The internal pipeline's
    // record is written from a spawn_blocking finalize task, so it can
    // arrive a few hundred ms after the stream closes. Polling avoids the
    // flake risk of a fixed sleep on slow CI.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let history = loop {
        let resp = server
            .get(&format!(
                "/v1/organizations/{}/usage/history?limit=10&offset=0",
                id.org_id
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(resp.status_code(), 200);
        let history: api::routes::usage::UsageHistoryResponse = resp.json();
        let matching = history
            .data
            .iter()
            .filter(|e| e.provider_request_id.as_deref() == Some(provider_id.as_str()))
            .count();
        if matching >= 2 || std::time::Instant::now() >= deadline {
            break history;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // Both records must land: one from the external submission, one from
    // the internal pipeline. If they share an inference_id, the second
    // INSERT silently no-ops via ON CONFLICT … DO NOTHING and only one
    // row survives.
    let entries_for_id: Vec<_> = history
        .data
        .iter()
        .filter(|e| e.provider_request_id.as_deref() == Some(provider_id.as_str()))
        .collect();
    assert_eq!(
        entries_for_id.len(),
        2,
        "expected one internal + one external record for provider id {provider_id:?}, got {} \
         (full history: {:?})",
        entries_for_id.len(),
        history.data,
    );

    // The two entries must have distinct inference_ids (different namespaces)
    // and the external one must be the cheap 1+1 token record.
    let inference_ids: std::collections::HashSet<_> = entries_for_id
        .iter()
        .filter_map(|e| e.inference_id.clone())
        .collect();
    assert_eq!(
        inference_ids.len(),
        2,
        "internal and external records must use disjoint inference_ids"
    );
    assert!(
        entries_for_id
            .iter()
            .any(|e| e.input_tokens == 1 && e.output_tokens == 1),
        "external 1+1-token record should be present"
    );
    assert!(
        entries_for_id
            .iter()
            .any(|e| e.input_tokens > 1 || e.output_tokens > 1),
        "internal pipeline record (with real token counts) should be present"
    );
}

/// Test that cache_read_tokens greater than input_tokens are rejected by validation.
#[tokio::test]
async fn test_record_chat_completion_usage_cache_read_capped_to_input() {
    let server = enable_internal_usage_server().await;

    // Setup model with cache-read pricing enabled
    let model = setup_qwen_model_with_cache_pricing(&server).await;

    let id = provision_identity(&server).await;

    // cache_read_tokens (100) > input_tokens (30) should be rejected by the API
    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": model,
            "input_tokens": 30,
            "output_tokens": 0,
            "cache_read_tokens": 100,
            "id": "test-chat-completion-cache-capped"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        400,
        "Usage recording with cache_read_tokens > input_tokens should return 400: {}",
        response.text()
    );
}

/// `POST /v1/internal/usage` returns 503 when the deployment has not
/// configured `CLOUD_API_USAGE_TOKEN`. This is the fail-closed posture —
/// the endpoint is disabled until an operator sets the secret.
#[tokio::test]
async fn test_internal_usage_returns_503_when_disabled() {
    let server = setup_test_server().await;

    let response = server
        .post("/v1/internal/usage")
        .add_header("Authorization", "Bearer any-token-here")
        .json(&serde_json::json!({
            "organization_id": "00000000-0000-0000-0000-000000000000",
            "workspace_id": "00000000-0000-0000-0000-000000000000",
            "api_key_id": "00000000-0000-0000-0000-000000000000",
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 10,
            "output_tokens": 20,
            "id": "test-internal-disabled"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        503,
        "Internal usage must 503 when CLOUD_API_USAGE_TOKEN is not configured: {}",
        response.text()
    );
}

/// `POST /v1/internal/usage` returns 503 even when the request omits the
/// `Authorization` header — the disabled-endpoint check runs *before*
/// the missing-header check, so callers see a single fail-closed
/// response regardless of what they sent. (The 401-on-mismatch path
/// is covered by `verify_internal_usage_token`'s unit tests; flipping
/// the harness config at runtime would race other tests.)
#[tokio::test]
async fn test_internal_usage_503_takes_precedence_over_missing_auth() {
    let server = setup_test_server().await;

    let response = server
        .post("/v1/internal/usage")
        .json(&serde_json::json!({
            "organization_id": "00000000-0000-0000-0000-000000000000",
            "workspace_id": "00000000-0000-0000-0000-000000000000",
            "api_key_id": "00000000-0000-0000-0000-000000000000",
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 1,
            "output_tokens": 1,
            "id": "test-internal-noauth"
        }))
        .await;

    assert_eq!(response.status_code(), 503);
}

/// Defense against extractor-ordering regressions: when the endpoint is
/// disabled, a request with a malformed body must still return 503 (not
/// 400/422). The handler takes raw `Bytes` and runs the token check
/// before attempting to deserialize, so body shape can't leak through
/// to unauthenticated callers.
#[tokio::test]
async fn test_internal_usage_503_takes_precedence_over_bad_body() {
    let server = setup_test_server().await;

    let response = server
        .post("/v1/internal/usage")
        .add_header("Authorization", "Bearer anything")
        .add_header("Content-Type", "application/json")
        .text("{not valid json")
        .await;

    assert_eq!(
        response.status_code(),
        503,
        "Disabled endpoint must short-circuit before body parsing: {}",
        response.text()
    );
}

/// Happy path with `internal_usage_token` configured: a valid request
/// records usage and returns the standard tagged-union response. Exercises
/// the actual `serde(flatten)` of `RecordUsageApiRequest` under the wrapper
/// — guards against "wire-format works in production for the first time
/// after a config flip on staging."
///
/// `api_key_id` comes from the workspace's `create_api_key` response so
/// the FK to `api_keys` resolves. This avoids depending on whatever
/// shape `/v1/check_api_key` is returning at the time the test runs
/// (PR #664 adds `api_key_id` there; until that lands the field is
/// just absent).
#[tokio::test]
async fn test_internal_usage_records_with_valid_token() {
    let server = enable_internal_usage_server().await;

    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-internal-happy-path"
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        200,
        "Authenticated internal request should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["type"], "chat_completion");
    assert_eq!(body["input_tokens"], 100);
    assert_eq!(body["output_tokens"], 50);
    // input: 100 * 1_000_000 + output: 50 * 2_000_000 = 200_000_000
    assert_eq!(body["total_cost"], 200_000_000i64);
}

/// `/v1/internal/usage` returns 401 when configured but called with the
/// wrong token. Pairs with the happy-path test; unit tests cover the
/// `verify_internal_usage_token` function directly but this asserts the
/// route handler wires it correctly.
#[tokio::test]
async fn test_internal_usage_returns_401_on_wrong_token() {
    let server = setup_test_server_with_config(|c| {
        c.internal_usage_token = Some("expected-secret".to_string());
    })
    .await;

    let response = server
        .post("/v1/internal/usage")
        .add_header("Authorization", "Bearer wrong-secret")
        .json(&serde_json::json!({
            "organization_id": "00000000-0000-0000-0000-000000000000",
            "workspace_id": "00000000-0000-0000-0000-000000000000",
            "api_key_id": "00000000-0000-0000-0000-000000000000",
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 1,
            "output_tokens": 1,
            "id": "test-internal-wrong-token"
        }))
        .await;

    assert_eq!(response.status_code(), 401);
}

/// `/v1/internal/usage` accepts the input-token-billed kinds (embedding,
/// rerank, score, privacy_classify) that inference-proxy now reports for
/// direct `sk-` requests. Each bills `input_tokens × input_rate` with no
/// output cost. See nearai/infra#169. (The internal ack echoes the
/// chat-completion shape; the *stored* `inference_type` carries the label.)
#[tokio::test]
async fn test_record_input_only_usage_types() {
    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    for (idx, ty) in ["embedding", "rerank", "score", "privacy_classify"]
        .iter()
        .enumerate()
    {
        let response = post_internal_usage(
            &server,
            &id,
            serde_json::json!({
                "type": ty,
                "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
                "input_tokens": 100,
                "id": format!("test-{ty}-{idx}"),
            }),
        )
        .await;

        assert_eq!(
            response.status_code(),
            200,
            "{ty} usage should record: {}",
            response.text()
        );

        let body: serde_json::Value = response.json();
        assert_eq!(body["input_tokens"], 100);
        assert_eq!(body["output_tokens"], 0);
        // 100 input tokens * 1_000_000 nano-dollars input rate; no output cost.
        assert_eq!(body["input_cost"], 100_000_000i64);
        assert_eq!(body["output_cost"], 0i64);
        assert_eq!(body["total_cost"], 100_000_000i64);
    }
}

/// The input-token-billed kinds reject a non-positive `input_tokens`.
#[tokio::test]
async fn test_record_input_only_usage_rejects_zero_tokens() {
    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "embedding",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 0,
            "id": "test-embedding-zero",
        }),
    )
    .await;

    assert_eq!(
        response.status_code(),
        400,
        "zero input_tokens should be rejected: {}",
        response.text()
    );
}

/// Fetch the org's usage history and return the entry recorded under
/// `provider_request_id`. The internal endpoint writes the row before it
/// answers, so no polling is needed.
async fn usage_history_entry(
    server: &axum_test::TestServer,
    org_id: &str,
    provider_request_id: &str,
) -> api::routes::usage::UsageHistoryEntryResponse {
    let resp = server
        .get(&format!(
            "/v1/organizations/{org_id}/usage/history?limit=20&offset=0"
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let history: api::routes::usage::UsageHistoryResponse = resp.json();
    history
        .data
        .into_iter()
        .find(|e| e.provider_request_id.as_deref() == Some(provider_request_id))
        .unwrap_or_else(|| panic!("no usage row recorded under {provider_request_id}"))
}

/// A trusted reporter can carry the `discount_to_user` an aggregator lane
/// publishes. The row is priced at list and then discounted on every
/// component; the list amounts stay in `billing_details` for auditing.
#[tokio::test]
async fn test_record_chat_completion_usage_with_discount() {
    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-chat-completion-discount-001",
            "discount_to_user": 0.2
        }),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();

    // List: 100 * 1_000_000 input + 50 * 2_000_000 output = 200_000_000; 20% off.
    assert_eq!(body["input_cost"], 80_000_000i64);
    assert_eq!(body["output_cost"], 80_000_000i64);
    assert_eq!(body["total_cost"], 160_000_000i64);
    assert_eq!(body["input_tokens"], 100);
    assert_eq!(body["output_tokens"], 50);

    let entry = usage_history_entry(&server, &id.org_id, "test-chat-completion-discount-001").await;
    assert_eq!(entry.total_cost, 160_000_000i64);
    let details = entry
        .billing_details
        .expect("discounted rows carry billing_details");
    assert_eq!(details["discount"]["discount_to_user"], 0.2);
    assert_eq!(details["discount"]["basis_points"], 2000);
    assert_eq!(details["discount"]["list_input_cost"], 100_000_000i64);
    assert_eq!(details["discount"]["list_output_cost"], 100_000_000i64);
    assert_eq!(details["discount"]["list_total_cost"], 200_000_000i64);
}

/// `discount_to_user: 0` (the aggregator's "no discount") records the row at
/// list price without a discount block.
#[tokio::test]
async fn test_internal_usage_discount_zero_bills_list_price() {
    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let response = post_internal_usage(
        &server,
        &id,
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-chat-completion-discount-zero",
            "discount_to_user": 0
        }),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: serde_json::Value = response.json();
    assert_eq!(body["total_cost"], 200_000_000i64);

    let entry =
        usage_history_entry(&server, &id.org_id, "test-chat-completion-discount-zero").await;
    assert!(
        entry
            .billing_details
            .map(|d| d.get("discount").is_none())
            .unwrap_or(true),
        "a zero discount must not leave a discount block"
    );
}

/// Discounts outside `[0, 1)`, finer than a basis point, or not a number are
/// rejected before anything is recorded.
#[tokio::test]
async fn test_internal_usage_rejects_invalid_discount() {
    let server = enable_internal_usage_server().await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    for (raw, why) in [
        (serde_json::json!(1.0), "100% off"),
        (serde_json::json!(-0.1), "negative"),
        (serde_json::json!(0.12345), "finer than a basis point"),
        (serde_json::json!("0.2"), "a string"),
    ] {
        let response = post_internal_usage(
            &server,
            &id,
            serde_json::json!({
                "type": "chat_completion",
                "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
                "input_tokens": 100,
                "output_tokens": 50,
                "id": format!("test-chat-completion-bad-discount-{why}"),
                "discount_to_user": raw
            }),
        )
        .await;
        assert_eq!(response.status_code(), 400, "{why}: {}", response.text());
    }

    // The org is fresh, so a rejected submission must leave its ledger empty.
    let resp = server
        .get(&format!(
            "/v1/organizations/{}/usage/history?limit=20&offset=0",
            id.org_id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let history: api::routes::usage::UsageHistoryResponse = resp.json();
    assert!(
        history.data.is_empty(),
        "rejected discounts must not record usage: {:?}",
        history.data.len()
    );
}

/// The operator ceiling (`INTERNAL_USAGE_MAX_DISCOUNT`) bounds what a
/// reporter can record: above it is a 400, at it is accepted.
#[tokio::test]
async fn test_internal_usage_rejects_discount_above_ceiling() {
    let server = setup_test_server_with_config(|c| {
        c.internal_usage_token = Some(INTERNAL_USAGE_TOKEN.to_string());
        c.internal_usage_max_discount = 0.1;
    })
    .await;
    setup_qwen_model(&server).await;
    let id = provision_identity(&server).await;

    let usage = |suffix: &str, discount: f64| {
        serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": format!("test-chat-completion-ceiling-{suffix}"),
            "discount_to_user": discount
        })
    };

    let rejected = post_internal_usage(&server, &id, usage("above", 0.2)).await;
    assert_eq!(rejected.status_code(), 400, "{}", rejected.text());
    assert!(
        rejected.text().contains("exceeds the configured maximum"),
        "{}",
        rejected.text()
    );

    let accepted = post_internal_usage(&server, &id, usage("at", 0.1)).await;
    assert_eq!(accepted.status_code(), 200, "{}", accepted.text());
    let body: serde_json::Value = accepted.json();
    assert_eq!(body["input_cost"], 90_000_000i64);
    assert_eq!(body["output_cost"], 90_000_000i64);
    assert_eq!(body["total_cost"], 180_000_000i64);
}
