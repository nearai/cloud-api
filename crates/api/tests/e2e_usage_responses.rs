//! E2E tests: call /v1/responses (Responses API) and verify usage is recorded,
//! including cache_read_tokens derived from provider-side cached_tokens.
//! Does not use the manual POST /v1/usage endpoint.

mod common;

use common::*;
use serde_json::json;
use services::responses::models::ResponseStreamEvent;

/// Helper: create a simple conversation for the given API key.
async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "name": "Test Conversation (usage)",
            "description": "Conversation for responses usage tests"
        }))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ConversationObject>()
}

/// Non-streaming Responses API: set mock cache_tokens based on provider token estimate,
/// then verify:
/// - ResponseObject.usage.input_tokens_details.cached_tokens equals that cache_tokens
/// - org usage history entry.cache_read_tokens matches the same value.
#[tokio::test]
async fn test_responses_non_stream_records_cache_usage_in_history() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;

    // Use cache-aware pricing so cache_read_tokens is meaningful in billing too
    setup_qwen_model_with_cache_pricing(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let message = "hello world from responses api";
    let estimated_tokens = message.split_whitespace().count() as i32;
    // Simple integer ratio: cache is roughly half of estimated input tokens.
    let cache_tokens = (estimated_tokens / 2).max(1);

    // Configure mock provider to report cached_tokens for this test
    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("cached reply (responses)")
                .with_cache_tokens(cache_tokens),
        )
        .await;

    // Create a conversation and then a non-streaming response
    let conversation = create_conversation(&server, api_key.clone()).await;

    let resp = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "conversation": { "id": conversation.id },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": 64,
            "stream": false,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507"
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        200,
        "Responses API (non-stream) should succeed: {}",
        resp.text()
    );

    let response_obj: api::models::ResponseObject = resp.json();
    assert!(matches!(
        response_obj.status,
        api::models::ResponseStatus::Completed
    ));

    // Verify usage on the ResponseObject itself
    let usage = &response_obj.usage;
    assert!(usage.input_tokens > 0, "input_tokens should be > 0");
    assert!(usage.output_tokens > 0, "output_tokens should be > 0");
    let cached_from_response: i32 = usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens as i32)
        .unwrap_or(0);
    assert_eq!(
        cached_from_response, cache_tokens,
        "ResponseObject.usage cached_tokens should equal configured cache_tokens"
    );

    // Allow async usage recording (ResponseService records usage after stream/agent loop)
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify org usage history reflects the same cache_read_tokens
    let history_resp = server
        .get(&format!(
            "/v1/organizations/{}/usage/history?limit=1&offset=0",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        history_resp.status_code(),
        200,
        "usage history should succeed: {}",
        history_resp.text()
    );

    let history: api::routes::usage::UsageHistoryResponse = history_resp.json();
    assert!(
        !history.data.is_empty(),
        "Should have usage history entries"
    );
    let entry = &history.data[0];
    assert_eq!(
        entry.cache_read_tokens, cache_tokens,
        "usage history should record cache_read_tokens consistent with ResponseObject"
    );

    // Also verify cost is consistent with tokens and pricing:
    // input_cost_per_token = 1_000_000, output_cost_per_token = 2_000_000, cache_read_cost_per_token = 500_000.
    const INPUT_PRICE: i64 = 1_000_000;
    const OUTPUT_PRICE: i64 = 2_000_000;
    const CACHE_PRICE: i64 = 500_000;
    let expected_total_cost = (entry.input_tokens as i64) * INPUT_PRICE
        + (entry.output_tokens as i64) * OUTPUT_PRICE
        + (entry.cache_read_tokens as i64) * CACHE_PRICE;
    assert_eq!(
        entry.total_cost, expected_total_cost,
        "total_cost should match input/output/cache tokens and configured pricing"
    );
}

/// Streaming Responses API: same cache setup as above, but with `stream: true`.
/// We consume the SSE stream (to ensure completion), then verify usage history
/// contains cache_read_tokens equal to that cache_tokens.
#[tokio::test]
async fn test_responses_stream_records_cache_usage_in_history() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;

    setup_qwen_model_with_cache_pricing(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let message = "hello world from responses api (stream)";
    let estimated_tokens = message.split_whitespace().count() as i32;
    let cache_tokens = (estimated_tokens / 2).max(1);

    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("cached reply (responses stream)")
                .with_cache_tokens(cache_tokens),
        )
        .await;

    let conversation = create_conversation(&server, api_key.clone()).await;

    let resp = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "conversation": { "id": conversation.id },
            "input": message,
            "temperature": 0.7,
            "max_output_tokens": 64,
            "stream": true,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507"
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        200,
        "Responses API (stream) should succeed: {}",
        resp.text()
    );

    // Drain SSE stream, parse final response.completed event to inspect usage
    let sse_text = resp.text();
    let mut completed_response: Option<services::responses::models::ResponseObject> = None;

    for chunk in sse_text.split("\n\n") {
        if chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in chunk.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event_type = name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if event_type == "response.completed" && !event_data.is_empty() {
            if let Ok(event) = serde_json::from_str::<ResponseStreamEvent>(event_data) {
                if let Some(resp_obj) = event.response {
                    completed_response = Some(resp_obj);
                }
            }
        }
    }

    let completed = completed_response.expect("Should capture final response from stream");
    let usage = completed.usage;
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "streaming response usage should have non-zero tokens"
    );
    let cached_from_stream: i32 = usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens as i32)
        .unwrap_or(0);
    assert_eq!(
        cached_from_stream, cache_tokens,
        "streaming ResponseObject usage cached_tokens should equal configured cache_tokens"
    );

    // Give ResponseService time to finalize and record usage
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let history_resp = server
        .get(&format!(
            "/v1/organizations/{}/usage/history?limit=1&offset=0",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        history_resp.status_code(),
        200,
        "usage history should succeed after streaming response"
    );

    let history: api::routes::usage::UsageHistoryResponse = history_resp.json();
    assert!(
        !history.data.is_empty(),
        "Should have usage history entries after streaming response"
    );
    let entry = &history.data[0];
    assert_eq!(
        entry.cache_read_tokens, cache_tokens,
        "cache_read_tokens should equal configured cache_tokens"
    );

    // Verify cost matches tokens and pricing (same prices as above).
    const INPUT_PRICE: i64 = 1_000_000;
    const OUTPUT_PRICE: i64 = 2_000_000;
    const CACHE_PRICE: i64 = 500_000;
    let expected_total_cost = (entry.input_tokens as i64) * INPUT_PRICE
        + (entry.output_tokens as i64) * OUTPUT_PRICE
        + (entry.cache_read_tokens as i64) * CACHE_PRICE;
    assert_eq!(
        entry.total_cost, expected_total_cost,
        "total_cost should match input/output/cache tokens and configured pricing for streaming response"
    );
}
