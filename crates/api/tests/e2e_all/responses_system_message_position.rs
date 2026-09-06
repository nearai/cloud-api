use crate::common::*;

/// The Codex CLI sends both a top-level `instructions` string and a
/// `developer`-role message as the first `input` item. Both become system
/// messages in the provider payload, and the provider rejects that with
/// "System message must be at the beginning." Before this was validated at
/// admission the request was answered with a 200 and the failure only arrived
/// as a `response.failed` event inside an already-committed SSE stream, which
/// clients cannot distinguish from a mid-stream disconnect.
#[tokio::test]
async fn non_leading_system_message_is_rejected_before_the_stream_opens() {
    // Given
    let (server, _pool, _mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // When
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "instructions": "You are a coding agent.",
            "input": [
                {"role": "developer", "content": "Repository guidelines."},
                {"role": "user", "content": "Fix the build."},
                {"role": "user", "content": "Then run the tests."}
            ],
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
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("System message must be at the beginning.")),
        "unexpected error body: {body}"
    );
}

#[tokio::test]
async fn user_only_input_is_still_accepted() {
    // Given
    let (server, _pool, _mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // When
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "instructions": "You are a coding agent.",
            "input": [{"role": "user", "content": "Fix the build."}],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 200, "{}", response.text());
}
