use crate::models::*;
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::conversations::ports::ConversationRequest;
use services::{ConversationError, ConversationId};
use std::sync::Arc;
use tracing::{debug};
use uuid::Uuid;

// Helper functions for ID conversion
fn parse_conversation_id(id_str: &str) -> Result<ConversationId, ConversationError> {
    // Handle both prefixed (conv_*) and raw UUID formats
    let uuid = if let Some(stripped) = id_str.strip_prefix("conv_") {
        Uuid::parse_str(stripped)
    } else {
        Uuid::parse_str(id_str)
    }
    .map_err(|_| ConversationError::InvalidParams(format!("Invalid conversation ID: {id_str}")))?;

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
///
/// Creates a new conversation for the authenticated user.
#[utoipa::path(
    post,
    path = "/conversations",
    tag = "Conversations",
    request_body = CreateConversationRequest,
    responses(
        (status = 201, description = "Conversation created successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_conversation(
    State(service): State<Arc<services::ConversationService>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    Json(request): Json<CreateConversationRequest>,
) -> Result<(StatusCode, ResponseJson<ConversationObject>), (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Create conversation request from key: {:?}", api_key);

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
        user_id: api_key.created_by_user_id.0.into(),
        metadata: request.metadata,
    };

    match service.create_conversation(domain_request.clone()).await {
        Ok(domain_conversation) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Created conversation {} for user {}",
                http_conversation.id, api_key.created_by_user_id.0
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
///
/// Returns details for a specific conversation.
#[utoipa::path(
    get,
    path = "/conversations/{conversation_id}",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation details", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn get_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Get conversation {} for user {}",
        conversation_id, api_key.created_by_user_id.0
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

    match service
        .get_conversation(
            &parsed_conversation_id,
            &api_key.created_by_user_id.0.into(),
        )
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
///
/// Updates a conversation's metadata.
#[utoipa::path(
    post,
    path = "/conversations/{conversation_id}",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    request_body = UpdateConversationRequest,
    responses(
        (status = 200, description = "Conversation updated successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn update_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    Json(request): Json<UpdateConversationRequest>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Update conversation {} for user {}",
        conversation_id, api_key.created_by_user_id.0
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

    let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

    match service
        .update_conversation(
            &parsed_conversation_id,
            &api_key.created_by_user_id.0.into(),
            metadata,
        )
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Updated conversation {} for user {}",
                conversation_id, api_key.created_by_user_id.0
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
///
/// Deletes a conversation permanently.
#[utoipa::path(
    delete,
    path = "/conversations/{conversation_id}",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation deleted successfully", body = ConversationDeleteResult),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn delete_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Delete conversation {} for user {}",
        conversation_id, api_key.created_by_user_id.0
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

    match service
        .delete_conversation(
            &parsed_conversation_id,
            &api_key.created_by_user_id.0.into(),
        )
        .await
    {
        Ok(true) => {
            debug!(
                "Deleted conversation {} for user {}",
                conversation_id, api_key.created_by_user_id.0
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

/// List items in a conversation (extracts from responses)
///
/// Returns items (messages, responses, etc.) within a specific conversation.
#[utoipa::path(
    get,
    path = "/conversations/{conversation_id}/items",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID"),
        ("limit" = Option<i64>, Query, description = "Maximum number of items to return"),
        ("offset" = Option<i64>, Query, description = "Number of items to skip")
    ),
    responses(
        (status = 200, description = "List of conversation items", body = ConversationItemList),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn list_conversation_items(
    Path(conversation_id): Path<String>,
    Query(params): Query<ListItemsQuery>,
    State(service): State<Arc<services::ConversationService>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "List items in conversation {} for user {}",
        conversation_id, api_key.created_by_user_id.0
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    match service
        .get_conversation_messages(
            &parsed_conversation_id,
            &api_key.created_by_user_id.0.into(),
            params.limit,
        )
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
        created_at: domain_conversation.created_at.timestamp(),
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
                format!("Internal server error: {msg}"),
                "internal_error".to_string(),
            ),
        }
    }
}

// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub order: Option<String>, // "asc" or "desc"
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
}
