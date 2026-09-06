use crate::common::*;
use inference_providers::MessageRole;

fn system_message_count(params: &inference_providers::ChatCompletionParams) -> usize {
    params
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .count()
}

fn message_text(message: &inference_providers::ChatMessage) -> &str {
    message
        .content
        .as_ref()
        .and_then(|content| content.as_str())
        .expect("message content should be text")
}

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
    assert_eq!(
        system_message_count(&params),
        1,
        "exactly one system message should reach the provider: {:?}",
        params.messages
    );
    assert_eq!(params.messages[0].role, MessageRole::System);

    let system_text = message_text(&params.messages[0]);
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
        .map(message_text)
        .collect();
    assert_eq!(user_texts, vec!["Fix the build.", "Then run the tests."]);
}

/// A system-level message that follows other content cannot be folded without
/// reordering what the model is told, so it is forwarded exactly as sent.
/// Whether that payload is acceptable is the provider's judgement — backends
/// differ — and refusing it here would generalize one template's constraint
/// into a gateway-wide rule.
#[tokio::test]
async fn system_message_after_other_content_is_forwarded_unchanged() {
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
                {"role": "user", "content": "Fix the build."},
                {"role": "system", "content": "Be concise."}
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
    let roles: Vec<_> = params.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![MessageRole::System, MessageRole::User, MessageRole::System],
        "the trailing system message must stay where the caller put it: {:?}",
        params.messages
    );
    assert_eq!(message_text(&params.messages[2]), "Be concise.");
}

/// Replayed conversation history sits ahead of the input items, so even a
/// leading system-level input message has content before it and is not folded.
/// It too is forwarded unchanged.
#[tokio::test]
async fn system_message_behind_replayed_history_is_forwarded_unchanged() {
    // Given
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Debugging"}))
        .await;
    assert_eq!(conversation.status_code(), 201, "{}", conversation.text());
    let conversation: api::models::ConversationObject = conversation.json();

    let first = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "conversation": {"id": conversation.id},
            "input": [{"role": "user", "content": "What broke?"}],
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;
    assert_eq!(first.status_code(), 200, "{}", first.text());

    // When
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "conversation": {"id": conversation.id},
            "input": [
                {"role": "developer", "content": "Repository guidelines."},
                {"role": "user", "content": "Fix the build."}
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
    let developer_index = params
        .messages
        .iter()
        .position(|message| {
            message.content.as_ref().and_then(|c| c.as_str()) == Some("Repository guidelines.")
        })
        .unwrap_or_else(|| {
            panic!(
                "the developer message should still be present: {:?}",
                params.messages
            )
        });
    assert!(
        developer_index > 0,
        "replayed history precedes it, so it is not folded into the leading \
         system message: {:?}",
        params.messages
    );
    assert_eq!(params.messages[developer_index].role, MessageRole::System);
    assert_eq!(
        system_message_count(&params),
        2,
        "the message is forwarded as its own system message: {:?}",
        params.messages
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
