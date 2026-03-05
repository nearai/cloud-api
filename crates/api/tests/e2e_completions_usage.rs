//! E2E tests: call /v1/chat/completions and verify usage in the response and that it was
//! recorded into the usage history table (including cache_read_tokens when present).
//! Does not use the manual POST /v1/usage endpoint.

mod common;

use common::*;
use serde_json::json;

/// Call chat/completions (non-streaming), assert usage in response, then verify org usage
/// history contains a matching entry (including cache_read_tokens).
#[tokio::test]
async fn test_chat_completions_records_usage_and_history() {
    let server = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let completion_resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                { "role": "user", "content": "hello" }
            ]
        }))
        .await;

    assert_eq!(
        completion_resp.status_code(),
        200,
        "chat/completions should succeed: {}",
        completion_resp.text()
    );

    let completion: api::models::ChatCompletionResponse = completion_resp.json();

    assert!(
        completion.usage.prompt_tokens > 0,
        "prompt_tokens should be > 0"
    );
    assert!(
        completion.usage.completion_tokens > 0,
        "completion_tokens should be > 0"
    );
    let cached_tokens: i32 = completion
        .usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens as i32)
        .unwrap_or(0);
    assert!(
        cached_tokens >= 0,
        "cached_tokens should be >= 0 (usually 0 for mocks)"
    );

    // Allow a short delay for async usage recording (completions record in Drop / spawn)
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
        entry.model, completion.model,
        "model in usage history should match completion model"
    );
    assert_eq!(
        entry.input_tokens, completion.usage.prompt_tokens,
        "input_tokens should match prompt_tokens"
    );
    assert_eq!(
        entry.output_tokens, completion.usage.completion_tokens,
        "output_tokens should match completion_tokens"
    );
    assert_eq!(
        entry.cache_read_tokens, cached_tokens,
        "cache_read_tokens should match prompt_tokens_details.cached_tokens (or 0)"
    );
    assert_eq!(
        entry.total_tokens,
        completion.usage.prompt_tokens + completion.usage.completion_tokens,
        "total_tokens should equal prompt_tokens + completion_tokens"
    );
}
