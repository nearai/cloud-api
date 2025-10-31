use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::ErrorResponse,
};
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
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::ports::ResponseServiceTrait;
use services::responses::service::ResponseServiceImpl;
use std::convert::Infallible;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

// Helper functions for error mapping
fn map_response_error_to_status(error: &ServiceResponseError) -> StatusCode {
    match error {
        ServiceResponseError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::UnknownTool(_) => StatusCode::BAD_REQUEST,
    }
}

impl From<ServiceResponseError> for ErrorResponse {
    fn from(error: ServiceResponseError) -> Self {
        ErrorResponse::new(error.to_string(), "response_error".to_string())
    }
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
    State(service): State<Arc<ResponseServiceImpl>>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Json(mut request): Json<CreateResponseRequest>,
) -> axum::response::Response {
    debug!(
        "Create response request from api key: {:?}",
        api_key.api_key.id
    );

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

    // Set defaults for internal fields
    request.max_tool_calls = request.max_tool_calls.or(Some(10));
    request.store = request.store.or(Some(true));
    request.background = request.background.or(Some(false));
    request.text = request.text.or(Some(ResponseTextConfig {
        format: ResponseTextFormat::Text,
    }));
    request.reasoning = request
        .reasoning
        .or(Some(ResponseReasoningConfig { effort: None }));

    // Store model for logging before moving request
    let model = request.model.clone();

    // Check if streaming is requested
    if request.stream.unwrap_or(false) {
        tracing::debug!(
            user_id = %api_key.api_key.created_by_user_id.0,
            model = %model,
            "Processing streaming response request"
        );

        // Create streaming response
        match service
            .create_response_stream(
                request,
                services::UserId(api_key.api_key.created_by_user_id.0),
                api_key.api_key.id.0.clone(),
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
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
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    error = %error,
                    "Failed to create streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    } else {
        tracing::debug!(
            user_id = %api_key.api_key.created_by_user_id.0,
            model = %model,
            "Processing non-streaming response request"
        );

        // Service only supports streaming - collect stream for non-streaming response
        match service
            .create_response_stream(
                request.clone(),
                services::UserId(api_key.api_key.created_by_user_id.0),
                api_key.api_key.id.0.clone(),
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    "Successfully created stream, collecting events for non-streaming response"
                );

                // Collect stream events to build complete response
                let mut response_id = None;
                let mut content = String::new();
                let mut status = ResponseStatus::InProgress;
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
                            // Extract response ID from response object
                            if let Some(response) = &event.response {
                                response_id = Some(response.id.clone());
                                tracing::debug!(
                                    "Non-streaming: extracted response_id={}",
                                    response.id
                                );
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
                            status = ResponseStatus::Completed;
                            tracing::debug!(
                                "Non-streaming: response.completed event, accumulated_content_len={}",
                                content.len()
                            );
                            // The response object is already in the right format
                            if let Some(response_obj) = event.response {
                                tracing::debug!(
                                    "Non-streaming: response.completed has response object"
                                );
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
                            status = ResponseStatus::Failed;
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
                        created_at: chrono::Utc::now().timestamp(),
                        status,
                        error: None,
                        incomplete_details: None,
                        instructions: request.instructions,
                        max_output_tokens: request.max_output_tokens,
                        max_tool_calls: request.max_tool_calls,
                        model: request.model,
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
                        previous_response_id: request.previous_response_id,
                        reasoning: None,
                        store: request.store.unwrap_or(false),
                        temperature: request.temperature.unwrap_or(0.7),
                        text: request.text,
                        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
                        tools: request.tools.unwrap_or_default(),
                        top_p: request.top_p.unwrap_or(1.0),
                        truncation: "stop".to_string(),
                        usage: Usage::new(0, 0), // TODO: Get actual usage from stream
                        user: None,
                        metadata: request.metadata,
                    }
                };

                debug!(
                    "Created response {} for key {}",
                    response.id, api_key.api_key.created_by_user_id.0
                );
                (StatusCode::OK, ResponseJson(response)).into_response()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
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
    State(_service): State<Arc<ResponseServiceImpl>>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
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
    State(_service): State<Arc<ResponseServiceImpl>>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
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
    State(_service): State<Arc<ResponseServiceImpl>>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
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
    State(_service): State<Arc<ResponseServiceImpl>>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
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

// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct GetResponseQuery {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub include_obfuscation: Option<bool>,
    pub starting_after: Option<i64>,
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListInputItemsQuery {
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
    pub limit: Option<i64>,
    pub order: Option<String>, // "asc" or "desc"
}
