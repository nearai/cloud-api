use crate::common::*;
use inference_providers::mock::{RequestMatcher, ResponseTemplate};

#[tokio::test]
async fn first_upstream_error_is_returned_as_http_error_before_sse_starts() {
    // Given
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_stream_error_override(Some(inference_providers::CompletionError::HttpError {
        status_code: 400,
        message: "Grammar error: Unimplemented keys: [\"uniqueItems\"]".to_string(),
        is_external: false,
    }))
    .await;

    // When
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Return JSON."}],
            "stream": true
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("uniqueItems")));
}

#[tokio::test]
async fn normal_first_upstream_chunk_remains_first_and_unmodified() {
    // Given
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.when(RequestMatcher::Any)
        .respond_with(ResponseTemplate::new("first second"))
        .await;

    // When
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Stream two words."}],
            "stream": true,
            "stream_options": {"continuous_usage_stats": true}
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let response_text = response.text();
    let first = response_text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("stream should contain a first data event");
    let chunk = serde_json::from_str::<serde_json::Value>(first).expect("valid first chunk");
    assert_eq!(chunk["choices"][0]["delta"]["content"], "first");
    assert_eq!(
        chunk["mock_upstream_only_field"], "dropped-by-typed-parse",
        "the provider's raw first chunk must bypass typed re-serialization"
    );
    assert!(chunk.get("error").is_none());
}
