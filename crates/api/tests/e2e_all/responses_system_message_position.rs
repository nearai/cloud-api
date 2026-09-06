use crate::common::*;
use inference_providers::MessageRole;

/// The Codex CLI sends both a top-level `instructions` string and a
/// `developer`-role message as the first `input` item. `to_chat_messages` maps
/// `developer` to `MessageRole::System`, so the provider payload used to carry
/// a second system message after the one derived from `instructions`, and
/// providers that require a leading system message rejected the whole request
/// — but only after the SSE stream had been committed with a 200, as a
/// `response.failed` event clients cannot tell from a mid-stream disconnect.
#[tokio::test]
async fn leading_developer_message_is_folded_into_the_system_message() {
    // Given
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
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
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let params = mock
        .last_chat_params()
        .await
        .expect("the provider should have been called");
    let system_messages = params
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .count();
    assert_eq!(
        system_messages, 1,
        "exactly one system message should reach the provider: {:?}",
        params.messages
    );
    assert_eq!(params.messages[0].role, MessageRole::System);

    let system_text = params.messages[0]
        .content
        .as_ref()
        .and_then(|content| content.as_str())
        .expect("system message should be text");
    assert!(
        system_text.starts_with("You are a coding agent."),
        "instructions should still lead the system message: {system_text}"
    );
    assert!(
        system_text.contains("Repository guidelines."),
        "developer content should be folded in, not dropped: {system_text}"
    );

    // The user turns keep their order and their role.
    let user_texts: Vec<_> = params
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| message.content.as_ref().and_then(|c| c.as_str()))
        .collect();
    assert_eq!(user_texts, vec!["Fix the build.", "Then run the tests."]);
}

/// A system-level message that follows other content cannot be folded without
/// reordering what the model is told, so it is refused at admission — while the
/// request is still an ordinary JSON response and not a committed stream.
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
                {"role": "user", "content": "Fix the build."},
                {"role": "system", "content": "Be concise."}
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
