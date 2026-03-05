//! E2E tests: call /v1/chat/completions and verify usage in the response and that it was
//! recorded into the usage history table (including cache_read_tokens when present).
//! Does not use the manual POST /v1/usage endpoint.

mod common;

use common::*;
use inference_providers::StreamChunk;
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

/// Call chat/completions with stream: true, consume the stream, then verify usage was
/// recorded in org usage history (limit=1).
#[tokio::test]
async fn test_chat_completions_stream_records_usage_in_history() {
    let server = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let stream_resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true
        }))
        .await;

    assert_eq!(
        stream_resp.status_code(),
        200,
        "streaming chat/completions should succeed: {}",
        stream_resp.text()
    );

    let text = stream_resp.text();
    let mut last_usage = None::<(i32, i32)>;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(StreamChunk::Chat(chat)) = serde_json::from_str::<StreamChunk>(data) {
                if let Some(usage) = &chat.usage {
                    last_usage = Some((usage.prompt_tokens, usage.completion_tokens));
                }
            }
        }
    }
    assert!(
        last_usage.is_some(),
        "stream should contain at least one chunk with usage"
    );
    let (prompt_tokens, completion_tokens) = last_usage.unwrap();
    assert!(prompt_tokens > 0 && completion_tokens > 0);

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
        "usage history should succeed"
    );
    let history: api::routes::usage::UsageHistoryResponse = history_resp.json();
    assert!(!history.data.is_empty(), "should have usage history entry");
    let entry = &history.data[0];
    assert_eq!(entry.input_tokens, prompt_tokens);
    assert_eq!(entry.output_tokens, completion_tokens);
    assert_eq!(
        entry.total_tokens,
        prompt_tokens + completion_tokens,
        "total_tokens should equal input + output"
    );
}

/// Use mock default response with cache_tokens; call completions and verify
/// cache_read_tokens in response and in usage history.
#[tokio::test]
async fn test_chat_completions_with_cache_records_cache_in_history() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;

    mock_provider
        .set_default_response(
            inference_providers::mock::ResponseTemplate::new("cached reply").with_cache_tokens(40),
        )
        .await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let completion_resp = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .await;

    assert_eq!(
        completion_resp.status_code(),
        200,
        "completions should succeed"
    );
    let completion: api::models::ChatCompletionResponse = completion_resp.json();
    let cached = completion
        .usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens as i32)
        .unwrap_or(0);
    assert_eq!(
        cached, 40,
        "response usage should have cache_read_tokens = 40"
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let history_resp = server
        .get(&format!(
            "/v1/organizations/{}/usage/history?limit=1&offset=0",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(history_resp.status_code(), 200);
    let history: api::routes::usage::UsageHistoryResponse = history_resp.json();
    assert!(!history.data.is_empty());
    let entry = &history.data[0];
    assert_eq!(
        entry.cache_read_tokens, 40,
        "usage history should record cache_read_tokens from completion"
    );
}
