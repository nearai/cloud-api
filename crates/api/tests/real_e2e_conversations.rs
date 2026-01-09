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

#[tokio::test]
async fn real_test_update_conversation_metadata() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;
    let _model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation = endpoints::create_conversation_with_metadata(
        &server,
        api_key.clone(),
        Some(serde_json::json!({
            "title": "Original Title",
            "description": "Should be removed",
            "source": "real_e2e"
        })),
    )
    .await;

    let update_response = server
        .post(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": {
                "title": "Updated Title",
                "context": "full replacement test"
            }
        }))
        .await;

    assert_eq!(
        update_response.status_code(),
        200,
        "Update conversation metadata should return 200"
    );

    let updated_conv = update_response.json::<api::models::ConversationObject>();
    let metadata_obj = updated_conv
        .metadata
        .as_object()
        .expect("Metadata should be an object");
    assert_eq!(
        metadata_obj.len(),
        2,
        "Metadata should only contain the new keys"
    );
    assert_eq!(
        metadata_obj
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Updated Title"
    );
    assert_eq!(
        metadata_obj
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "full replacement test"
    );
    assert!(
        metadata_obj.get("description").is_none(),
        "Old metadata keys should be removed when updating"
    );
}

#[tokio::test]
async fn real_test_delete_conversation() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;
    let _model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation = endpoints::create_conversation_with_metadata(
        &server,
        api_key.clone(),
        Some(serde_json::json!({ "title": "To be deleted" })),
    )
    .await;

    let pre_delete = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        pre_delete.status_code(),
        200,
        "Conversation should exist before delete"
    );

    let delete_response = server
        .delete(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        delete_response.status_code(),
        200,
        "Delete conversation should succeed"
    );

    let get_after_delete = server
        .get(format!("/v1/conversations/{}", conversation.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        get_after_delete.status_code(),
        404,
        "Deleted conversation should no longer be accessible"
    );
}
