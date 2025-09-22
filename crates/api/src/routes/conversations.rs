use crate::{conversions::authenticated_user_to_user_id, middleware::AuthenticatedUser, models::*};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::conversations::ports::ConversationRequest;
use services::{ConversationError, ConversationId, UserId};
use std::sync::Arc;
use tracing::{debug, info};
use uuid::Uuid;

// Helper functions for ID conversion
fn parse_conversation_id(id_str: &str) -> Result<ConversationId, ConversationError> {
    // Handle both prefixed (conv_*) and raw UUID formats
    let uuid = if id_str.starts_with("conv_") {
        Uuid::parse_str(&id_str[5..])
    } else {
        Uuid::parse_str(id_str)
    }
    .map_err(|_| {
        ConversationError::InvalidParams(format!("Invalid conversation ID: {}", id_str))
    })?;

    Ok(ConversationId::from(uuid))
}

// Add a function to handle error mapping for ConversationError
fn map_conversation_error_to_status(error: &ConversationError) -> StatusCode {
    match error {
        ConversationError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ConversationError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Create a new conversation
pub async fn create_conversation(
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateConversationRequest>,
) -> Result<(StatusCode, ResponseJson<ConversationObject>), (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Create conversation request from user: {}", user.0.id);

    // Validate the request
    if let Err(error) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let domain_request = ConversationRequest {
        user_id: authenticated_user_to_user_id(user),
        metadata: request.metadata,
    };

    match service.create_conversation(domain_request.clone()).await {
        Ok(domain_conversation) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            info!(
                "Created conversation {} for user {}",
                http_conversation.id, domain_request.user_id
            );
            Ok((StatusCode::CREATED, ResponseJson(http_conversation)))
        }
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Get a conversation by ID
pub async fn get_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Get conversation {} for user {}",
        conversation_id, user.0.id
    );

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    let user_id = authenticated_user_to_user_id(user);

    match service
        .get_conversation(&parsed_conversation_id, &user_id)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            Ok(ResponseJson(http_conversation))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Update a conversation
pub async fn update_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<UpdateConversationRequest>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Update conversation {} for user {}",
        conversation_id, user.0.id
    );

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    let user_id = authenticated_user_to_user_id(user);
    let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

    match service
        .update_conversation(&parsed_conversation_id, &user_id, metadata)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            info!(
                "Updated conversation {} for user {}",
                conversation_id, user_id
            );
            Ok(ResponseJson(http_conversation))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Delete a conversation
pub async fn delete_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Delete conversation {} for user {}",
        conversation_id, user.0.id
    );

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    let user_id = authenticated_user_to_user_id(user);

    match service
        .delete_conversation(&parsed_conversation_id, &user_id)
        .await
    {
        Ok(true) => {
            info!(
                "Deleted conversation {} for user {}",
                conversation_id, user_id
            );
            Ok(ResponseJson(ConversationDeleteResult {
                id: conversation_id,
                object: "conversation.deleted".to_string(),
                deleted: true,
            }))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// List conversations
pub async fn list_conversations(
    Query(params): Query<ListConversationsQuery>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "List conversations for user {} with limit={:?}, offset={:?}",
        user.0.id, params.limit, params.offset
    );

    let user_id = authenticated_user_to_user_id(user);

    match service
        .list_conversations(&user_id, params.limit, params.offset)
        .await
    {
        Ok(domain_conversations) => {
            let http_conversations: Vec<ConversationObject> = domain_conversations
                .into_iter()
                .map(convert_domain_conversation_to_http)
                .collect();

            let first_id = http_conversations
                .first()
                .map(|c| c.id.clone())
                .unwrap_or_default();
            let last_id = http_conversations
                .last()
                .map(|c| c.id.clone())
                .unwrap_or_default();

            // Determine if there are more conversations
            let has_more = params
                .limit
                .map_or(false, |limit| http_conversations.len() >= limit as usize);

            Ok(ResponseJson(ConversationList {
                object: "list".to_string(),
                data: http_conversations,
                first_id,
                last_id,
                has_more,
            }))
        }
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// List items in a conversation (extracts from responses)
pub async fn list_conversation_items(
    Path(conversation_id): Path<String>,
    Query(params): Query<ListItemsQuery>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "List items in conversation {} for user {}",
        conversation_id, user.0.id
    );

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    let user_id = authenticated_user_to_user_id(user);

    match service
        .get_conversation_messages(&parsed_conversation_id, &user_id, params.limit)
        .await
    {
        Ok(messages) => {
            let http_items: Vec<ConversationItem> = messages
                .into_iter()
                .map(|msg| ConversationItem::Message {
                    id: msg.id.to_string(),
                    status: ResponseItemStatus::Completed,
                    role: msg.role,
                    content: vec![ConversationContentPart::InputText { text: msg.content }],
                    metadata: msg.metadata,
                })
                .collect();

            let first_id = http_items.first().map(get_item_id).unwrap_or_default();
            let last_id = http_items.last().map(get_item_id).unwrap_or_default();

            Ok(ResponseJson(ConversationItemList {
                object: "list".to_string(),
                data: http_items,
                first_id,
                last_id,
                has_more: false,
            }))
        }
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

// Helper functions

fn convert_domain_conversation_to_http(
    domain_conversation: services::conversations::ports::Conversation,
) -> ConversationObject {
    ConversationObject {
        id: domain_conversation.id.to_string(),
        object: "conversation".to_string(),
        created_at: domain_conversation.created_at.timestamp() as u64,
        metadata: domain_conversation.metadata,
    }
}

fn get_item_id(item: &ConversationItem) -> String {
    match item {
        ConversationItem::Message { id, .. } => id.clone(),
    }
}

// Add conversion from ConversationError to ErrorResponse
impl From<ConversationError> for ErrorResponse {
    fn from(err: ConversationError) -> Self {
        match err {
            ConversationError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            ConversationError::InternalError(msg) => ErrorResponse::new(
                format!("Internal server error: {}", msg),
                "internal_error".to_string(),
            ),
        }
    }
}

// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct ListConversationsQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    pub limit: Option<i32>,
    pub order: Option<String>, // "asc" or "desc"
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
}
