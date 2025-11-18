use crate::models::*;
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::{
    conversations::{errors::ConversationError, models::ConversationId},
    responses::models::TextAnnotation,
};
use std::sync::Arc;
use tracing::debug;
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

/// Create conversation
///
/// Create a new conversation to organize chat messages.
#[utoipa::path(
    post,
    path = "/v1/conversations",
    tag = "Conversations",
    request_body = CreateConversationRequest,
    responses(
        (status = 201, description = "Conversation created", body = ConversationObject),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_conversation(
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
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

    // Parse API key ID from string to UUID
    let api_key_uuid = uuid::Uuid::parse_str(&api_key.id.0).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                format!("Invalid API key ID format: {e}"),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    let domain_request = services::conversations::models::ConversationRequest {
        workspace_id: api_key.workspace_id.clone(),
        api_key_id: api_key_uuid,
        metadata: request.metadata,
    };

    match service.create_conversation(domain_request.clone()).await {
        Ok(domain_conversation) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Created conversation {} for workspace {}",
                http_conversation.id, api_key.workspace_id.0
            );
            Ok((StatusCode::CREATED, ResponseJson(http_conversation)))
        }
        Err(error) => Err((
            map_conversation_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Get conversation
///
/// Retrieve conversation details by ID.
#[utoipa::path(
    get,
    path = "/v1/conversations/{conversation_id}",
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
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Get conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .get_conversation(parsed_conversation_id, api_key.workspace_id.clone())
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

/// Update conversation
///
/// Update conversation metadata.
#[utoipa::path(
    post,
    path = "/v1/conversations/{conversation_id}",
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
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    Json(request): Json<UpdateConversationRequest>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Update conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
            parsed_conversation_id,
            api_key.workspace_id.clone(),
            metadata,
        )
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Updated conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Delete conversation
///
/// Delete a conversation and all its messages.
#[utoipa::path(
    delete,
    path = "/v1/conversations/{conversation_id}",
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
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Delete conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .delete_conversation(parsed_conversation_id, api_key.workspace_id.clone())
        .await
    {
        Ok(true) => {
            debug!(
                "Deleted conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Pin a conversation
///
/// Pins a conversation to keep it at the top of the list.
#[utoipa::path(
    post,
    path = "/conversations/{conversation_id}/pin",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation pinned successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn pin_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Pin conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .pin_conversation(parsed_conversation_id, api_key.workspace_id.clone(), true)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Pinned conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Unpin a conversation
///
/// Unpins a conversation.
#[utoipa::path(
    delete,
    path = "/conversations/{conversation_id}/pin",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation unpinned successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn unpin_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Unpin conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .pin_conversation(parsed_conversation_id, api_key.workspace_id.clone(), false)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Unpinned conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Archive a conversation
///
/// Archives a conversation to hide it from the main list.
#[utoipa::path(
    post,
    path = "/conversations/{conversation_id}/archive",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation archived successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn archive_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Archive conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .archive_conversation(parsed_conversation_id, api_key.workspace_id.clone(), true)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Archived conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Unarchive a conversation
///
/// Unarchives a conversation to show it in the main list again.
#[utoipa::path(
    delete,
    path = "/conversations/{conversation_id}/archive",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation unarchived successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn unarchive_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Unarchive conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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
        .archive_conversation(parsed_conversation_id, api_key.workspace_id.clone(), false)
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Unarchived conversation {} for workspace {}",
                conversation_id, api_key.workspace_id.0
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

/// Clone a conversation
///
/// Creates a copy of an existing conversation with a new ID.
#[utoipa::path(
    post,
    path = "/conversations/{conversation_id}/clone",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 201, description = "Conversation cloned successfully", body = ConversationObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn clone_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<(StatusCode, ResponseJson<ConversationObject>), (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Clone conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
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

    // Parse API key ID from string to UUID
    let api_key_uuid = uuid::Uuid::parse_str(&api_key.id.0).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                format!("Invalid API key ID format: {e}"),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    match service
        .clone_conversation(
            parsed_conversation_id,
            api_key.workspace_id.clone(),
            api_key_uuid,
        )
        .await
    {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            debug!(
                "Cloned conversation {} -> {} for workspace {}",
                conversation_id, http_conversation.id, api_key.workspace_id.0
            );
            Ok((StatusCode::CREATED, ResponseJson(http_conversation)))
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

/// List conversation messages
///
/// Get all messages and responses in a conversation, sorted by creation time.
#[utoipa::path(
    get,
    path = "/v1/conversations/{conversation_id}/items",
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
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ConversationItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "List items in conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
    );

    // Validate limit parameter
    if params.limit <= 0 || params.limit > 1000 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Limit must be between 1 and 1000".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    // Request limit + 1 items to determine if there are more
    let fetch_limit = params.limit + 1;

    // Get items from conversation service
    match service
        .list_conversation_items(
            parsed_conversation_id,
            api_key.workspace_id.clone(),
            params.after.clone(),
            fetch_limit,
        )
        .await
    {
        Ok(items) => {
            // Convert ResponseOutputItems to ConversationItems
            let http_items: Vec<ConversationItem> = items
                .into_iter()
                .map(convert_output_item_to_conversation_item)
                .collect();

            // Now check has_more and truncate AFTER filtering
            let has_more = http_items.len() > params.limit as usize;
            let http_items = if has_more {
                http_items.into_iter().take(params.limit as usize).collect()
            } else {
                http_items
            };

            let first_id = http_items.first().map(get_item_id).unwrap_or_default();
            let last_id = http_items.last().map(get_item_id).unwrap_or_default();

            Ok(ResponseJson(ConversationItemList {
                object: "list".to_string(),
                data: http_items,
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

/// Create items in a conversation (for backfilling)
///
/// Adds items to a conversation, allowing API callers to backfill conversations.
#[utoipa::path(
    post,
    path = "/v1/conversations/{conversation_id}/items",
    tag = "Conversations",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID"),
        ("include" = Option<Vec<String>>, Query, description = "Additional fields to include in the response")
    ),
    request_body = CreateConversationItemsRequest,
    responses(
        (status = 200, description = "Items created successfully", body = ConversationItemList),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Conversation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_conversation_items(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<dyn services::conversations::ports::ConversationServiceTrait>>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    Json(request): Json<CreateConversationItemsRequest>,
) -> Result<ResponseJson<ConversationItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Create items in conversation {} for workspace {}",
        conversation_id, api_key.workspace_id.0
    );

    // Validate items count (max 20 items per request)
    if request.items.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Items array cannot be empty".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    if request.items.len() > 20 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Cannot add more than 20 items at a time".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let parsed_conversation_id = match parse_conversation_id(&conversation_id) {
        Ok(id) => id,
        Err(error) => {
            return Err((
                map_conversation_error_to_status(&error),
                ResponseJson(error.into()),
            ))
        }
    };

    // Parse API key ID from string to UUID
    let api_key_uuid = uuid::Uuid::parse_str(&api_key.id.0).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                format!("Invalid API key ID format: {e}"),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    // Convert input items to response output items
    let response_items: Vec<services::responses::models::ResponseOutputItem> = request
        .items
        .into_iter()
        .map(convert_input_item_to_response_item)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to convert input item: {e}"),
                    "invalid_request_error".to_string(),
                )),
            )
        })?;

    // Create items via service
    match service
        .create_conversation_items(
            parsed_conversation_id,
            api_key.workspace_id.clone(),
            api_key_uuid,
            response_items,
        )
        .await
    {
        Ok(created_items) => {
            // Convert response items back to conversation items
            let http_items: Vec<ConversationItem> = created_items
                .into_iter()
                .map(convert_output_item_to_conversation_item)
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

fn convert_input_item_to_response_item(
    item: ConversationInputItem,
) -> Result<services::responses::models::ResponseOutputItem, String> {
    match item {
        ConversationInputItem::Message { role, content, .. } => {
            // Convert ConversationContent to ResponseOutputContent
            let response_content = match content {
                ConversationContent::Text(text) => {
                    vec![
                        services::responses::models::ResponseOutputContent::OutputText {
                            text: text.trim().to_string(),
                            annotations: vec![],
                            logprobs: vec![],
                        },
                    ]
                }
                ConversationContent::Parts(parts) => {
                    parts
                        .into_iter()
                        .filter_map(|part| match part {
                            ConversationContentPart::InputText { text } => Some(
                                services::responses::models::ResponseOutputContent::OutputText {
                                    text: text.trim().to_string(),
                                    annotations: vec![],
                                    logprobs: vec![],
                                },
                            ),
                            ConversationContentPart::InputImage { .. } => {
                                // TODO: Handle image content
                                None
                            }
                            ConversationContentPart::OutputText { text, .. } => Some(
                                services::responses::models::ResponseOutputContent::OutputText {
                                    text: text.trim().to_string(),
                                    annotations: vec![],
                                    logprobs: vec![],
                                },
                            ),
                        })
                        .collect()
                }
            };

            if response_content.is_empty() {
                return Err("Message content cannot be empty".to_string());
            }

            Ok(services::responses::models::ResponseOutputItem::Message {
                id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                response_id: String::new(), // Will be enriched by repository
                previous_response_id: None, // Will be enriched by repository
                next_response_ids: vec![],  // Will be enriched by repository
                created_at: 0,              // Will be enriched by repository
                status: services::responses::models::ResponseItemStatus::Completed,
                role,
                content: response_content,
                model: String::new(), // Will be enriched by repository
            })
        }
    }
}

fn convert_output_item_to_conversation_item(
    item: services::responses::models::ResponseOutputItem,
) -> ConversationItem {
    use services::responses::models::ResponseOutputItem;

    match item {
        ResponseOutputItem::Message {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status,
            role,
            content,
            model,
        } => {
            // Convert ResponseOutputContent to ConversationContentPart
            // For user messages, use input_text; for assistant/system, use output_text
            let is_user_message = role == "user";
            let conv_content: Vec<ConversationContentPart> = content
                .into_iter()
                .filter_map(|c| match c {
                    services::responses::models::ResponseOutputContent::OutputText {
                        text,
                        annotations,
                        logprobs: _,
                    } => {
                        if is_user_message {
                            // User messages should use input_text format
                            Some(ConversationContentPart::InputText { text })
                        } else {
                            // Assistant/system messages use output_text format
                            Some(ConversationContentPart::OutputText {
                                text,
                                annotations: Some(
                                    annotations
                                        .into_iter()
                                        .map(convert_text_annotation)
                                        .map(|a| serde_json::to_value(a).unwrap())
                                        .collect(),
                                ),
                            })
                        }
                    }
                    _ => None,
                })
                .collect();

            ConversationItem::Message {
                id,
                response_id,
                previous_response_id,
                next_response_ids,
                created_at,
                status: convert_response_item_status(status),
                role,
                content: conv_content,
                metadata: None,
                model,
            }
        }
        ResponseOutputItem::ToolCall {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status,
            tool_type,
            function,
            model,
        } => ConversationItem::ToolCall {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status: convert_response_item_status(status),
            tool_type,
            function: ConversationItemFunction {
                name: function.name,
                arguments: function.arguments,
            },
            model,
        },
        ResponseOutputItem::WebSearchCall {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status,
            action,
            model,
        } => ConversationItem::WebSearchCall {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status: convert_response_item_status(status),
            action: match action {
                services::responses::models::WebSearchAction::Search { query } => {
                    ConversationItemWebSearchAction::Search { query }
                }
            },
            model,
        },
        ResponseOutputItem::Reasoning {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status,
            summary,
            content,
            model,
        } => ConversationItem::Reasoning {
            id,
            response_id,
            previous_response_id,
            next_response_ids,
            created_at,
            status: convert_response_item_status(status),
            summary,
            content,
            model,
        },
    }
}

fn convert_text_annotation(
    annotation: services::responses::models::TextAnnotation,
) -> TextAnnotation {
    match annotation {
        services::responses::models::TextAnnotation::UrlCitation {
            start_index,
            end_index,
            title,
            url,
        } => TextAnnotation::UrlCitation {
            start_index,
            end_index,
            title,
            url,
        },
    }
}

fn convert_response_item_status(
    status: services::responses::models::ResponseItemStatus,
) -> ResponseItemStatus {
    match status {
        services::responses::models::ResponseItemStatus::Completed => ResponseItemStatus::Completed,
        services::responses::models::ResponseItemStatus::Failed => ResponseItemStatus::Failed,
        services::responses::models::ResponseItemStatus::InProgress => {
            ResponseItemStatus::InProgress
        }
        services::responses::models::ResponseItemStatus::Cancelled => ResponseItemStatus::Cancelled,
    }
}

fn convert_domain_conversation_to_http(
    domain_conversation: services::conversations::models::Conversation,
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
        ConversationItem::ToolCall { id, .. } => id.clone(),
        ConversationItem::WebSearchCall { id, .. } => id.clone(),
        ConversationItem::Reasoning { id, .. } => id.clone(),
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
                "internal_server_error".to_string(),
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
