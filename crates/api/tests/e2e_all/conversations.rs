//! Conversations API coverage while Responses inference manages no server-side history.

use crate::common::*;
use api::models::{ConversationContentPart, ConversationItem};
use uuid::Uuid;

mod cloning;
mod history;
mod items;
mod management;
mod validation;

async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: String,
) -> api::models::ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation"
        }))
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<api::models::ConversationObject>()
}

async fn list_conversation_items(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> api::models::ConversationItemList {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::ConversationItemList>()
}

/// Seed a pre-existing response through the repositories rather than the stateless API.
/// Conversation read/clone tests still need realistic encrypted history and parent links.
async fn seed_response(
    database: &std::sync::Arc<database::Database>,
    conversation_id: &str,
    model: &str,
    input: &str,
    previous_response_id: Option<&str>,
    confidential: bool,
) -> services::responses::models::ResponseObject {
    use services::responses::{
        models,
        ports::{ResponseItemRepositoryTrait, ResponseRepositoryTrait},
    };

    let conversation_id: services::conversations::models::ConversationId =
        conversation_id.parse().unwrap();
    let row = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT workspace_id, api_key_id FROM conversations WHERE id = $1",
            &[&conversation_id.0],
        )
        .await
        .unwrap();
    let workspace = services::workspace::WorkspaceId(row.get(0));
    let key: Uuid = row.get(1);
    let mut request = serde_json::json!({
        "model": model,
        "input": input,
        "conversation": conversation_id.to_string(),
        "previous_response_id": previous_response_id,
        "store": true,
    });
    if confidential {
        request["instructions"] = serde_json::json!("private system instruction");
        request["metadata"] = serde_json::json!({"private": "response metadata"});
    }
    let response = database
        .responses
        .create(
            workspace.clone(),
            key,
            serde_json::from_value(request).unwrap(),
        )
        .await
        .unwrap();
    let response_id =
        models::ResponseId(response.id.strip_prefix("resp_").unwrap().parse().unwrap());
    for (role, content) in [
        (
            "user",
            vec![models::ResponseContentItem::InputText {
                text: input.to_string(),
            }],
        ),
        (
            "assistant",
            vec![models::ResponseContentItem::OutputText {
                text: format!("Retained answer to {input}"),
                annotations: vec![],
                logprobs: vec![],
            }],
        ),
    ] {
        database
            .response_items
            .create(
                response_id.clone(),
                key,
                Some(conversation_id),
                models::ResponseOutputItem::Message {
                    id: format!("msg_{}", Uuid::new_v4().simple()),
                    response_id: response.id.clone(),
                    previous_response_id: response.previous_response_id.clone(),
                    next_response_ids: vec![],
                    created_at: response.created_at,
                    status: models::ResponseItemStatus::Completed,
                    role: role.to_string(),
                    content,
                    model: model.to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
    }
    database
        .responses
        .update(
            response_id,
            workspace,
            Some(format!("Retained answer to {input}")),
            models::ResponseStatus::Completed,
            None,
        )
        .await
        .unwrap()
        .unwrap()
}
