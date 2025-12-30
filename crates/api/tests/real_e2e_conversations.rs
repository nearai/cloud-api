mod common;

use common::endpoints;
use common::*;

use api::models::{ConversationContentPart, ConversationItem};

#[tokio::test]
async fn real_test_conversation_items_populated_by_responses_non_stream() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;

    // Seed model metadata/pricing into DB so /v1/responses passes validation/usage logic.
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation = endpoints::create_conversation_with_metadata(
        &server,
        api_key.clone(),
        Some(serde_json::json!({ "source": "real_e2e" })),
    )
    .await;
    let message = "Reply with exactly the word: ok".to_string();

    let resp = endpoints::create_response_with_temperature(
        &server,
        conversation.id.clone(),
        model_name,
        message.clone(),
        32,
        api_key.clone(),
        0.2,
    )
    .await;

    assert_eq!(resp.status, api::models::ResponseStatus::Completed);

    let items = endpoints::list_conversation_items(&server, conversation.id, api_key).await;

    // We expect at least the user message to be persisted; assistant may include extra items depending on provider.
    assert!(
        items.data.iter().any(|it| match it {
            ConversationItem::Message { role, content, .. } if role == "user" =>
                content.iter().any(|p| {
                    matches!(p, ConversationContentPart::InputText { text } if text == &message)
                }),
            _ => false,
        }),
        "Expected conversation items to include the user input message"
    );

    assert!(
        items.data.iter().any(|it| match it {
            ConversationItem::Message { role, .. } => role == "assistant",
            _ => false,
        }),
        "Expected at least one assistant message item in conversation"
    );
}

#[tokio::test]
async fn real_test_conversation_items_populated_by_responses_streaming() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;

    // Seed model metadata/pricing into DB so /v1/responses passes validation/usage logic.
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation = endpoints::create_conversation_with_metadata(
        &server,
        api_key.clone(),
        Some(serde_json::json!({ "source": "real_e2e" })),
    )
    .await;
    let message = "Write a one-word reply: ok".to_string();

    let (_streamed_text, final_resp) = endpoints::create_response_stream_with_temperature(
        &server,
        conversation.id.clone(),
        model_name,
        message.clone(),
        64,
        api_key.clone(),
        0.2,
    )
    .await;

    assert_eq!(final_resp.status, api::models::ResponseStatus::Completed);

    let items = endpoints::list_conversation_items(&server, conversation.id, api_key).await;
    assert!(
        !items.data.is_empty(),
        "Expected conversation items to be present after streaming response"
    );

    assert!(
        items.data.iter().any(|it| match it {
            ConversationItem::Message { role, content, .. } if role == "user" =>
                content.iter().any(|p| {
                    matches!(p, ConversationContentPart::InputText { text } if text == &message)
                }),
            _ => false,
        }),
        "Expected conversation items to include the user input message"
    );
}
