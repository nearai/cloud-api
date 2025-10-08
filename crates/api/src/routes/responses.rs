use crate::models::*;
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Json as ResponseJson,
    },
};
use futures::stream::StreamExt;
use serde::Deserialize;
use services::{
    ConversationId, ResponseError, ResponseId, ResponseInput as DomainResponseInput,
    ResponseMessage, ResponseRequest, ResponseService, ResponseStatus as DomainResponseStatus,
    UserId,
};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::{debug, info};
use uuid::Uuid;

// Helper function to map ResponseError to HTTP status code
fn map_response_error_to_status(error: &ResponseError) -> StatusCode {
    match error {
        ResponseError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ResponseError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// Add conversion from ResponseError to ErrorResponse
impl From<ResponseError> for ErrorResponse {
    fn from(err: ResponseError) -> Self {
        match err {
            ResponseError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            ResponseError::InternalError(msg) => ErrorResponse::new(
                format!("Internal server error: {}", msg),
                "internal_error".to_string(),
            ),
        }
    }
}

// Helper functions for ID conversion
fn parse_response_id(id_str: &str) -> Result<ResponseId, ResponseError> {
    let uuid = if let Some(stripped) = id_str.strip_prefix("resp_") {
        Uuid::parse_str(stripped)
    } else {
        Uuid::parse_str(id_str)
    }
    .map_err(|_| ResponseError::InvalidParams(format!("Invalid response ID: {}", id_str)))?;

    Ok(ResponseId::from(uuid))
}

fn parse_conversation_id_from_string(id_str: &str) -> Result<ConversationId, ResponseError> {
    let uuid = if let Some(stripped) = id_str.strip_prefix("conv_") {
        Uuid::parse_str(stripped)
    } else {
        Uuid::parse_str(id_str)
    }
    .map_err(|_| ResponseError::InvalidParams(format!("Invalid conversation ID: {}", id_str)))?;

    Ok(ConversationId::from(uuid))
}

fn user_uuid_to_user_id(uuid: Uuid) -> UserId {
    UserId::from(uuid)
}

/// Create a new response
///
/// Creates a new response for a conversation.
#[utoipa::path(
    post,
    path = "/responses",
    tag = "Responses",
    request_body = CreateResponseRequest,
    responses(
        (status = 200, description = "Response created successfully", body = ResponseObject),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_response(
    State(service): State<Arc<ResponseService>>,
    Extension(api_key): Extension<services::auth::ApiKey>,
    Json(request): Json<CreateResponseRequest>,
) -> axum::response::Response {
    debug!("Create response request from key: {:?}", api_key);

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Convert HTTP request to domain request
    let domain_input = request.input.clone().map(|input| match input {
        ResponseInput::Text(text) => DomainResponseInput::Text(text),
        ResponseInput::Items(items) => {
            let messages = items
                .into_iter()
                .map(|item| match item {
                    ResponseInputItem::Message { role, content } => {
                        let text = match content {
                            ResponseContent::Text(t) => t,
                            ResponseContent::Parts(parts) => {
                                // Extract text from parts
                                parts
                                    .into_iter()
                                    .filter_map(|part| match part {
                                        ResponseContentPart::InputText { text } => Some(text),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            }
                        };
                        ResponseMessage {
                            role,
                            content: text,
                        }
                    }
                })
                .collect();
            DomainResponseInput::Messages(messages)
        }
    });

    let domain_request = ResponseRequest {
        model: request.model.clone(),
        input: domain_input,
        instructions: request.instructions.clone(),
        conversation_id: request.conversation.clone().and_then(|c| match c {
            ConversationReference::Id(id) => parse_conversation_id_from_string(&id).ok(),
            ConversationReference::Object { id, .. } => parse_conversation_id_from_string(&id).ok(),
        }),
        previous_response_id: request
            .previous_response_id
            .clone()
            .and_then(|id| parse_response_id(&id).ok()),
        max_output_tokens: request.max_output_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        user_id: user_uuid_to_user_id(api_key.created_by_user_id.0),
        metadata: request.metadata.clone(),
    };

    // Check if streaming is requested
    if request.stream.unwrap_or(false) {
        tracing::info!(
            user_id = %api_key.created_by_user_id.0,
            model = %request.model,
            "Processing streaming response request"
        );

        // Create streaming response
        match service.create_response_stream(domain_request).await {
            Ok(stream) => {
                tracing::info!(
                    user_id = %api_key.created_by_user_id.0,
                    "Successfully created streaming response, returning SSE stream"
                );

                let sse_stream = stream.map(|event| {
                    Ok::<_, Infallible>(
                        Event::default()
                            .event(event.event_type.clone())
                            .data(serde_json::to_string(&event).unwrap_or_default()),
                    )
                });

                // Return SSE response
                Sse::new(sse_stream)
                    .keep_alive(axum::response::sse::KeepAlive::default())
                    .into_response()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.created_by_user_id.0,
                    model = %request.model,
                    error = %error,
                    "Failed to create streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    } else {
        tracing::info!(
            user_id = %api_key.created_by_user_id.0,
            model = %request.model,
            "Processing non-streaming response request"
        );

        // Service only supports streaming - collect stream for non-streaming response
        match service.create_response_stream(domain_request).await {
            Ok(stream) => {
                tracing::info!(
                    user_id = %api_key.created_by_user_id.0,
                    "Successfully created stream, collecting events for non-streaming response"
                );

                // Collect stream events to build complete response
                let mut response_id = None;
                let mut content = String::new();
                let mut status = DomainResponseStatus::InProgress;
                let mut final_response: Option<ResponseObject> = None;

                let mut stream = Box::pin(stream);
                let mut event_count = 0;
                let mut delta_count = 0;
                while let Some(event) = stream.next().await {
                    event_count += 1;
                    tracing::debug!(
                        "Non-streaming collection: received event #{} type={} delta={:?}",
                        event_count,
                        event.event_type,
                        event.delta
                    );
                    match event.event_type.as_str() {
                        "response.created" => {
                            // Extract response ID from JSON response object
                            if let Some(response) = &event.response {
                                if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                                    response_id = Some(id.to_string());
                                    tracing::debug!("Non-streaming: extracted response_id={}", id);
                                }
                            }
                        }
                        "response.output_text.delta" => {
                            // Accumulate content deltas
                            if let Some(delta) = &event.delta {
                                delta_count += 1;
                                tracing::debug!(
                                    "Non-streaming: delta #{} len={} content='{}'",
                                    delta_count,
                                    delta.len(),
                                    delta
                                );
                                content.push_str(delta);
                            }
                        }
                        "response.completed" => {
                            status = DomainResponseStatus::Completed;
                            tracing::debug!(
                                "Non-streaming: response.completed event, accumulated_content_len={}",
                                content.len()
                            );
                            // Convert the JSON response to a ResponseObject
                            if let Some(response_json) = event.response {
                                tracing::debug!(
                                    "Non-streaming: response.completed has response JSON: {:?}",
                                    response_json
                                );
                                if let Ok(response_obj) =
                                    serde_json::from_value::<ResponseObject>(response_json)
                                {
                                    tracing::debug!(
                                        "Non-streaming: parsed ResponseObject, checking output text"
                                    );
                                    // Log the output text from the final response
                                    for (idx, output_item) in response_obj.output.iter().enumerate()
                                    {
                                        if let ResponseOutputItem::Message {
                                            content: msg_content,
                                            ..
                                        } = output_item
                                        {
                                            for (cidx, content_part) in
                                                msg_content.iter().enumerate()
                                            {
                                                if let ResponseOutputContent::OutputText {
                                                    text,
                                                    ..
                                                } = content_part
                                                {
                                                    tracing::debug!(
                                                        "Non-streaming: final_response output[{}].content[{}] text_len={} text='{}'",
                                                        idx, cidx, text.len(), text
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    final_response = Some(response_obj);
                                }
                            }
                        }
                        "response.failed" => {
                            status = DomainResponseStatus::Failed;
                        }
                        _ => {
                            // Handle other events as needed
                        }
                    }
                }
                tracing::info!(
                    "Non-streaming: collected {} events, {} deltas, accumulated_content_len={}",
                    event_count,
                    delta_count,
                    content.len()
                );

                // Use final response from completed event or build fallback response
                let response = if let Some(final_resp) = final_response {
                    // Use the complete response object from the response.completed event
                    final_resp
                } else {
                    // Fallback: Build response from collected data (for compatibility)
                    ResponseObject {
                        id: response_id.unwrap_or_else(|| format!("resp_{}", Uuid::new_v4())),
                        object: "response".to_string(),
                        created_at: chrono::Utc::now().timestamp() as u64,
                        status: match status {
                            DomainResponseStatus::InProgress => ResponseStatus::InProgress,
                            DomainResponseStatus::Completed => ResponseStatus::Completed,
                            DomainResponseStatus::Failed => ResponseStatus::Failed,
                            DomainResponseStatus::Cancelled => ResponseStatus::Cancelled,
                        },
                        error: None,
                        incomplete_details: None,
                        instructions: request.instructions.clone(),
                        max_output_tokens: request.max_output_tokens,
                        max_tool_calls: request.max_tool_calls,
                        model: request.model.clone(),
                        output: vec![ResponseOutputItem::Message {
                            id: format!("msg_{}", Uuid::new_v4()),
                            status: ResponseItemStatus::Completed,
                            role: "assistant".to_string(),
                            content: vec![ResponseOutputContent::OutputText {
                                text: content,
                                annotations: vec![],
                            }],
                        }],
                        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
                        previous_response_id: request.previous_response_id.clone(),
                        reasoning: None,
                        store: request.store.unwrap_or(false),
                        temperature: request.temperature.unwrap_or(0.7),
                        text: request.text.clone(),
                        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
                        tools: request.tools.clone().unwrap_or_default(),
                        top_p: request.top_p.unwrap_or(1.0),
                        truncation: "stop".to_string(),
                        usage: Usage::new(0, 0), // TODO: Get actual usage from stream
                        user: None,
                        metadata: request.metadata.clone(),
                    }
                };

                info!(
                    "Created response {} for key {}",
                    response.id, api_key.created_by_user_id.0
                );
                (StatusCode::OK, ResponseJson(response)).into_response()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.created_by_user_id.0,
                    model = %request.model,
                    error = %error,
                    "Failed to create non-streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    }
}

/// Get a response by ID
pub async fn get_response(
    Path(_response_id): Path<String>,
    Query(_params): Query<GetResponseQuery>,
    State(_service): State<Arc<ResponseService>>,
    Extension(_api_key): Extension<services::auth::ApiKey>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement get_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Get response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// Delete a response
pub async fn delete_response(
    Path(_response_id): Path<String>,
    State(_service): State<Arc<ResponseService>>,
    Extension(_api_key): Extension<services::auth::ApiKey>,
) -> Result<ResponseJson<ResponseDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement delete_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Delete response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// Cancel a response (for background responses)
pub async fn cancel_response(
    Path(_response_id): Path<String>,
    State(_service): State<Arc<ResponseService>>,
    Extension(_api_key): Extension<services::auth::ApiKey>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement cancel_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Cancel response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// List input items for a response (simplified implementation)
pub async fn list_input_items(
    Path(_response_id): Path<String>,
    Query(_params): Query<ListInputItemsQuery>,
    State(_service): State<Arc<ResponseService>>,
    Extension(_api_key): Extension<services::auth::ApiKey>,
) -> Result<ResponseJson<ResponseInputItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement get_response method in ResponseService to support listing input items
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "List input items not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

// Helper functions

#[allow(dead_code)]
fn convert_domain_response_to_http_with_request(
    domain_response: services::Response,
    request: &CreateResponseRequest,
) -> ResponseObject {
    let status = match domain_response.status {
        DomainResponseStatus::InProgress => ResponseStatus::InProgress,
        DomainResponseStatus::Completed => ResponseStatus::Completed,
        DomainResponseStatus::Failed => ResponseStatus::Failed,
        DomainResponseStatus::Cancelled => ResponseStatus::Cancelled,
    };

    let output = if let Some(output_text) = domain_response.output_message {
        vec![ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            status: ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![ResponseOutputContent::OutputText {
                text: output_text,
                annotations: vec![],
            }],
        }]
    } else {
        vec![]
    };

    ResponseObject {
        id: domain_response.id.to_string(),
        object: "response".to_string(),
        created_at: domain_response.created_at.timestamp() as u64,
        status,
        error: None,
        incomplete_details: None,
        instructions: domain_response.instructions,
        max_output_tokens: request.max_output_tokens,
        max_tool_calls: request.max_tool_calls,
        model: domain_response.model,
        output,
        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
        previous_response_id: domain_response
            .previous_response_id
            .map(|id| id.to_string()),
        reasoning: Some(ResponseReasoningOutput {
            effort: None,
            summary: None,
        }),
        store: request.store.unwrap_or(true),
        temperature: request.temperature.unwrap_or(1.0),
        text: request.text.clone().or(Some(ResponseTextConfig {
            format: ResponseTextFormat::Text,
        })),
        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
        tools: request.tools.clone().unwrap_or_default(),
        top_p: request.top_p.unwrap_or(1.0),
        truncation: "disabled".to_string(),
        usage: domain_response
            .usage
            .map(|u| Usage {
                input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                input_tokens_details: Some(InputTokensDetails { cached_tokens: 0 }),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                output_tokens_details: Some(OutputTokensDetails {
                    reasoning_tokens: 0,
                }),
                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            })
            .unwrap_or(Usage::new(10, 20)),
        user: None,
        metadata: domain_response
            .metadata
            .or_else(|| Some(serde_json::json!({}))),
    }
}

// Simple conversion function for endpoints that don't have request context
#[allow(dead_code)]
fn convert_domain_response_to_http_simple(domain_response: services::Response) -> ResponseObject {
    let status = match domain_response.status {
        DomainResponseStatus::InProgress => ResponseStatus::InProgress,
        DomainResponseStatus::Completed => ResponseStatus::Completed,
        DomainResponseStatus::Failed => ResponseStatus::Failed,
        DomainResponseStatus::Cancelled => ResponseStatus::Cancelled,
    };

    let output = if let Some(output_text) = domain_response.output_message {
        vec![ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            status: ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![ResponseOutputContent::OutputText {
                text: output_text,
                annotations: vec![],
            }],
        }]
    } else {
        vec![]
    };

    ResponseObject {
        id: domain_response.id.to_string(),
        object: "response".to_string(),
        created_at: domain_response.created_at.timestamp() as u64,
        status,
        error: None,
        incomplete_details: None,
        instructions: domain_response.instructions,
        max_output_tokens: None,
        max_tool_calls: None,
        model: domain_response.model,
        output,
        parallel_tool_calls: true,
        previous_response_id: domain_response
            .previous_response_id
            .map(|id| id.to_string()),
        reasoning: Some(ResponseReasoningOutput {
            effort: None,
            summary: None,
        }),
        store: true,
        temperature: 1.0,
        text: Some(ResponseTextConfig {
            format: ResponseTextFormat::Text,
        }),
        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
        tools: vec![],
        top_p: 1.0,
        truncation: "disabled".to_string(),
        usage: domain_response
            .usage
            .map(|u| Usage {
                input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                input_tokens_details: Some(InputTokensDetails { cached_tokens: 0 }),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                output_tokens_details: Some(OutputTokensDetails {
                    reasoning_tokens: 0,
                }),
                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            })
            .unwrap_or(Usage::new(10, 20)),
        user: None,
        metadata: domain_response
            .metadata
            .or_else(|| Some(serde_json::json!({}))),
    }
}

// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct GetResponseQuery {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub include_obfuscation: Option<bool>,
    pub starting_after: Option<i32>,
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListInputItemsQuery {
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
    pub limit: Option<i32>,
    pub order: Option<String>, // "asc" or "desc"
}
