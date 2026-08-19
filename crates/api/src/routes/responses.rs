use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash, RequestCorrelation},
    models::ErrorResponse,
    routes::extractors::OpenAiJson,
};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::{IntoResponse, Json as ResponseJson},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::ports::ResponseServiceTrait;
use services::responses::service::ResponseServiceImpl;
use std::convert::Infallible;
use std::sync::Arc;
use tracing::debug;

// Helper functions for error mapping
fn map_response_error_to_status(error: &ServiceResponseError) -> StatusCode {
    match error {
        ServiceResponseError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::Completion(error) => {
            crate::routes::common::map_domain_error_to_status(error)
        }
        ServiceResponseError::UnknownTool(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::EmptyToolName => StatusCode::BAD_REQUEST,
        ServiceResponseError::StreamInterrupted => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::ConversationNotFound => StatusCode::NOT_FOUND,
        ServiceResponseError::PreviousResponseNotFound => StatusCode::NOT_FOUND,
        ServiceResponseError::McpConnectionFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpToolDiscoveryFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpToolExecutionFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpServerLimitExceeded { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpToolLimitExceeded { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpInsecureUrl => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpPrivateIpBlocked => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpApprovalRequired { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpApprovalRequestNotFound(_) => StatusCode::NOT_FOUND,
        ServiceResponseError::FunctionCallRequired { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::FunctionCallNotFound(_) => StatusCode::NOT_FOUND,
    }
}

fn status_code_from_response_event(status_code: Option<u16>) -> StatusCode {
    status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn error_response_from_response_event(
    error: services::responses::models::ResponseError,
) -> ErrorResponse {
    let mut response = ErrorResponse::new(error.message, error.type_);
    response.error.param = error.param;
    response.error.code = error.code;
    response
}

fn with_no_store_cache_header(mut response: axum::response::Response) -> axum::response::Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

impl From<ServiceResponseError> for ErrorResponse {
    fn from(error: ServiceResponseError) -> Self {
        match error {
            ServiceResponseError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            ServiceResponseError::Completion(error) => error.into(),
            ServiceResponseError::InternalError(msg) => ErrorResponse::new(
                format!("Internal server error: {msg}"),
                "internal_server_error".to_string(),
            ),
            ServiceResponseError::UnknownTool(msg) => ErrorResponse::new(
                format!("Unknown tool: {msg}"),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::EmptyToolName => ErrorResponse::new(
                "Tool call is missing a tool name".to_string(),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::StreamInterrupted => {
                ErrorResponse::new("Stream interrupted".to_string(), "stream_error".to_string())
            }
            ServiceResponseError::ConversationNotFound => ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::PreviousResponseNotFound => ErrorResponse::new(
                "Previous response not found".to_string(),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::McpConnectionFailed(msg) => ErrorResponse::new(
                format!("MCP connection failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpToolDiscoveryFailed(msg) => ErrorResponse::new(
                format!("MCP tool discovery failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpToolExecutionFailed(msg) => ErrorResponse::new(
                format!("MCP tool execution failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpServerLimitExceeded { max } => ErrorResponse::new(
                format!("MCP server limit exceeded: max {max} servers per request"),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpToolLimitExceeded { server, count, max } => {
                ErrorResponse::new(
                    format!(
                        "MCP tool limit exceeded: server '{server}' has {count} tools, max {max}"
                    ),
                    "invalid_request_error".to_string(),
                )
            }
            ServiceResponseError::McpInsecureUrl => ErrorResponse::new(
                "MCP server URL must use HTTPS".to_string(),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpPrivateIpBlocked => ErrorResponse::new(
                "MCP private IP addresses not allowed".to_string(),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpApprovalRequired { server, tool } => ErrorResponse::new(
                format!("MCP approval required for tool '{tool}' on server '{server}'"),
                "mcp_approval_required".to_string(),
            ),
            ServiceResponseError::McpApprovalRequestNotFound(msg) => ErrorResponse::new(
                format!("MCP approval request not found: {msg}"),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::FunctionCallRequired { name, call_id } => ErrorResponse::new(
                format!("Function call required: {name} (call_id: {call_id})"),
                "function_call_required".to_string(),
            ),
            ServiceResponseError::FunctionCallNotFound(msg) => ErrorResponse::new(
                format!("Function call not found: {msg}"),
                "not_found_error".to_string(),
            ),
        }
    }
}

// State for response routes
#[derive(Clone)]
pub struct ResponseRouteState {
    pub response_service: Arc<ResponseServiceImpl>,
}

/// Return an explicit migration response for the retired response-history API.
///
/// The route is mounted behind the standard API-key middleware.  Keeping it
/// separate from the create endpoint makes it clear that only a new, single
/// stateless request is supported.
pub async fn response_history_gone() -> axum::response::Response {
    with_no_store_cache_header(
        (
            StatusCode::GONE,
            ResponseJson(ErrorResponse::new(
                "Response history is unavailable because the Responses API is stateless."
                    .to_string(),
                "gone".to_string(),
            )),
        )
            .into_response(),
    )
}

/// Create response
///
/// Generate a single-turn, stateless AI response with optional streaming.
#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "Responses",
    request_body = CreateResponseRequest,
    responses(
        (status = 200, description = "Response created", body = ResponseObject),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_response(
    State(state): State<ResponseRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: HeaderMap,
    OpenAiJson(mut request): OpenAiJson<CreateResponseRequest>,
) -> axum::response::Response {
    let service = state.response_service.clone();
    debug!(
        "Create response request from api key: {:?}",
        api_key.api_key.id
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return with_no_store_cache_header(
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    error,
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response(),
        );
    }

    if let Err(error) = request.validate_stateless() {
        return with_no_store_cache_header(
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    error,
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response(),
        );
    }

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return with_no_store_cache_header(err.into_response()),
    };

    let signing_algo = encryption_headers.signing_algo;
    let client_pub_key = encryption_headers.client_pub_key;
    let model_pub_key = encryption_headers.model_pub_key;
    let encryption_version = encryption_headers.encryption_version;

    // Encryption requires streaming mode because encrypted chunks from vLLM are independently
    // encrypted and cannot be concatenated. Non-streaming mode would produce corrupted data.
    if signing_algo.is_some() && client_pub_key.is_some() && request.stream != Some(true) {
        return with_no_store_cache_header(
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Non-streaming mode is not supported with encryption. Use stream=true."
                        .to_string(),
                    "encryption_requires_streaming".to_string(),
                )),
            )
                .into_response(),
        );
    }

    // Set defaults for internal fields
    request.max_tool_calls = request.max_tool_calls.or(Some(10));
    request.store = Some(false);
    request.background = request.background.or(Some(false));
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
                correlation.request_id,
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
                signing_algo.clone(),
                client_pub_key.clone(),
                model_pub_key.clone(),
                encryption_version.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    "Successfully created streaming response"
                );

                // Format events as SSE without retaining the stream payload or
                // writing a response attestation. A no-store response has no
                // durable response record to associate with such data.
                let byte_stream = stream.map(|event| {
                    let json = serde_json::to_string(&event).expect("event serialization failed");
                    let sse_bytes = format!("event: {}\ndata: {}\n\n", event.event_type, json);
                    Ok::<Bytes, Infallible>(Bytes::from(sse_bytes))
                });

                // Return as raw byte stream with SSE headers
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-store")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    error = %error,
                    "Failed to create streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                with_no_store_cache_header(
                    (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response(),
                )
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
                correlation.request_id,
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
                signing_algo.clone(),
                client_pub_key.clone(),
                model_pub_key.clone(),
                encryption_version.clone(),
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
                let mut tracked_usage: Option<Usage> = None;
                let mut failed_error: Option<services::responses::models::ResponseError> = None;
                let mut failed_status_code: Option<u16> = None;

                let mut stream = Box::pin(stream);
                let mut event_count = 0;
                let mut delta_count = 0;
                while let Some(event) = stream.next().await {
                    event_count += 1;
                    let event_type = event.event_type.as_str();
                    let delta_len = event.delta.as_ref().map_or(0, String::len);
                    let has_delta = event.delta.is_some();
                    tracing::debug!(
                        event_count,
                        event_type,
                        has_delta,
                        delta_len,
                        "Non-streaming collection received event"
                    );
                    match event_type {
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
                                    "Non-streaming: delta #{} len={}",
                                    delta_count,
                                    delta.len()
                                );
                                content.push_str(delta);
                            }
                        }
                        "response.completed" => {
                            status = ResponseStatus::Completed;
                            if event.usage.is_some() {
                                tracked_usage = event.usage.clone();
                            }
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
                                                if let ResponseContentItem::OutputText {
                                                    text,
                                                    ..
                                                } = content_part
                                                {
                                                    tracing::debug!(
                                                        "Non-streaming: final_response output[{}].content[{}] text_len={}",
                                                        idx, cidx, text.len()
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
                            failed_error = event.error.clone();
                            failed_status_code = event.status_code;
                            if event.usage.is_some() {
                                tracked_usage = event.usage.clone();
                            }
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

                if final_response.is_none() {
                    if let Some(error) = failed_error {
                        let status_code = status_code_from_response_event(failed_status_code);
                        let error_response = error_response_from_response_event(error);
                        return with_no_store_cache_header(
                            (status_code, ResponseJson(error_response)).into_response(),
                        );
                    }
                }

                // Use final response from completed event or build fallback response
                let response = if let Some(final_resp) = final_response {
                    // Use the complete response object from the response.completed event
                    final_resp
                } else {
                    // Fallback: Build response from collected data (for compatibility)
                    // Trim accumulated content to remove leading/trailing whitespace
                    let trimmed_content = content.trim().to_string();
                    let resp_id = response_id
                        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
                    ResponseObject {
                        id: resp_id.clone(),
                        object: "response".to_string(),
                        created_at: chrono::Utc::now().timestamp(),
                        status,
                        background: false,
                        conversation: None,
                        error: None,
                        incomplete_details: None,
                        instructions: request.instructions,
                        max_output_tokens: request.max_output_tokens,
                        max_tool_calls: request.max_tool_calls,
                        model: request.model.clone(),
                        output: vec![ResponseOutputItem::Message {
                            id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                            response_id: resp_id.clone(),
                            previous_response_id: None,
                            next_response_ids: vec![],
                            created_at: chrono::Utc::now().timestamp(),
                            status: ResponseItemStatus::Completed,
                            role: "assistant".to_string(),
                            content: vec![ResponseContentItem::OutputText {
                                text: trimmed_content,
                                annotations: vec![],
                                logprobs: vec![],
                            }],
                            model: request.model,
                            metadata: None,
                        }],
                        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
                        previous_response_id: None,
                        next_response_ids: vec![],
                        prompt_cache_key: request.prompt_cache_key,
                        prompt_cache_retention: None,
                        reasoning: None,
                        safety_identifier: request.safety_identifier,
                        service_tier: "default".to_string(),
                        store: false,
                        temperature: request.temperature.unwrap_or(1.0),
                        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
                        tools: request.tools.unwrap_or_default(),
                        top_logprobs: 0,
                        top_p: request.top_p.unwrap_or(1.0),
                        truncation: "disabled".to_string(),
                        usage: tracked_usage.unwrap_or_else(|| Usage::new(0, 0)),
                        user: None,
                        metadata: request.metadata,
                    }
                };

                debug!(
                    "Created response {} for key {}",
                    response.id, api_key.api_key.created_by_user_id.0
                );

                with_no_store_cache_header((StatusCode::OK, ResponseJson(response)).into_response())
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    error = %error,
                    "Failed to create non-streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                with_no_store_cache_header(
                    (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_results_are_marked_no_store() {
        let response = with_no_store_cache_header(StatusCode::OK.into_response());
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
    }

    #[tokio::test]
    async fn response_history_is_explicitly_gone() {
        let response = response_history_gone().await;
        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
    }
}
